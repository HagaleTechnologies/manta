//! MAN-33: RST and QRL? spot-content annotations. Orthogonal to the
//! validation pipeline (ARCHITECTURE §6 steps 1-5) -- these ride on an
//! already-valid spot and gate nothing.

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::{Spot, Validator};

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

/// Real telemetry, so `try_spot`'s `has_meta` gate (MAN-28 round 8)
/// doesn't hold back every spot in tests that don't otherwise care
/// about metadata timing.
fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
    });
}

#[test]
fn v31_spot_carries_the_most_recently_decoded_rst() {
    // Given a track's decoded text contains a recognizable RST pattern
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    // When manta-spot emits a spot for that track
    let spots = run(
        &transmission_events(1, &["TU", "5NN", "CQ", "K5ARH", "CQ", "K5ARH"], 0),
        &mut v,
    );
    // Then the spot carries the most recently decoded RST value
    assert_eq!(spots.len(), 1, "spots were {spots:?}");
    assert_eq!(spots[0].rst.as_deref(), Some("599"));
    assert!(!spots[0].qrl_query);
}

#[test]
fn v31_a_later_rst_replaces_an_earlier_one() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let spots = run(
        &transmission_events(1, &["5NN", "TU", "339", "CQ", "K5ARH", "CQ", "K5ARH"], 0),
        &mut v,
    );
    assert_eq!(spots[0].rst.as_deref(), Some("339"), "last value must win");
}

#[test]
fn v31_rst_survives_aging_out_of_the_word_window() {
    // "most recently decoded", not "currently in the 16-word window" --
    // the annotation lives on TrackState, not on a Word (plan decision D4).
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let mut words = vec!["5NN"];
    words.extend(std::iter::repeat_n("TEST", 17)); // clippy rejects a push loop here
    words.extend(["CQ", "K5ARH", "CQ", "K5ARH"]);
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(spots[0].rst.as_deref(), Some("599"));
}

#[test]
fn v32_spot_is_flagged_when_the_track_sent_a_qrl_query() {
    // Given a track's decoded text contains "QRL?"
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    // When manta-spot emits a spot for that track
    let spots = run(
        &transmission_events(1, &["QRL?", "QRL?", "CQ", "K5ARH", "CQ", "K5ARH"], 0),
        &mut v,
    );
    // Then the spot is flagged as a QRL? request
    assert_eq!(spots.len(), 1, "spots were {spots:?}");
    assert!(spots[0].qrl_query);
    assert!(spots[0].rst.is_none());
}

#[test]
fn v32_a_bare_qrl_response_is_not_flagged_as_a_query() {
    // Review finding (PR #159): `QRL` without the `?` says the frequency IS
    // in use -- the answer to the question, not the question. It must not
    // set `qrl_query`.
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let spots = run(
        &transmission_events(1, &["QRL", "QRL", "CQ", "K5ARH", "CQ", "K5ARH"], 0),
        &mut v,
    );
    assert_eq!(spots.len(), 1, "spots were {spots:?}");
    assert!(!spots[0].qrl_query);
}

#[test]
fn v32_an_ordinary_cq_is_not_flagged() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let spots = run(
        &transmission_events(1, &["CQ", "K5ARH", "CQ", "K5ARH"], 0),
        &mut v,
    );
    assert!(!spots[0].qrl_query);
    assert!(spots[0].rst.is_none());
}

#[test]
fn track_closed_clears_both_annotations() {
    // Annotations are per track and must not leak into a reused track_id --
    // the same teardown invariant MAN-19 established for words and the gate.
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    run(&transmission_events(1, &["QRL?", "5NN"], 0), &mut v);
    v.ingest(&DecoderEvent::TrackClosed {
        track_id: 1,
        closure: ClosureKind::SignalEnded,
    });
    seed_meta(&mut v, 1);
    let spots = run(
        &transmission_events(1, &["CQ", "K5ARH", "CQ", "K5ARH"], 200_000),
        &mut v,
    );
    assert!(spots[0].rst.is_none() && !spots[0].qrl_query);
}
