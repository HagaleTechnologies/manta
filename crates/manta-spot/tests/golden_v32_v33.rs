//! MAN-272: the latest RST and a QRL? flag carried on a spot (SPEC §7.1
//! V32/V33). Both are per-track annotations read from the emitting track
//! when the spot is built; neither gates spotting, and a track merge does
//! not carry the loser's annotations to the survivor (decision D5 in
//! docs/DECISIONS/2026-10-16-man272-rst-and-qrl-on-spots.md).

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::{Spot, SpotType, Validator};

const FS: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for c in text.chars() {
        events.push(DecoderEvent::char_decoded(
            track_id,
            ts,
            Glyph::Char(c),
            0.95,
        ));
        ts += 100;
    }
    events.push(DecoderEvent::word_boundary(track_id, ts));
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

/// The same transmission at `t0` and again at `t0 + 100_000`: under the
/// default `normal` validation level a single transmission is one message,
/// so a spot needs the second (MAN-100).
fn twice(v: &mut Validator, track_id: u32, words: &[&str], t0: u64) -> Vec<Spot> {
    let mut spots = run(&transmission_events(track_id, words, t0), v);
    spots.extend(run(&transmission_events(track_id, words, t0 + 100_000), v));
    spots
}

/// Real telemetry, so `try_spot`'s `has_meta` gate (MAN-28 round 8) doesn't
/// hold back every spot in tests that don't otherwise care about metadata
/// timing.
fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        sample_ts: 0,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
    });
}

fn validator() -> Validator {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v
}

#[test]
fn v32_spot_carries_the_most_recently_decoded_rst() {
    // Given a track's decoded text contains a recognizable RST such as "5NN"
    let mut v = validator();

    // When manta emits a spot for that track
    let spots = twice(&mut v, 1, &["TU", "5NN", "DE", "K5ARH", "K"], 0);

    // Then the spot carries the most recently decoded RST value
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].callsign, "K5ARH");
    assert_eq!(spots[0].rst.as_deref(), Some("599"));
    assert!(!spots[0].qrl_query);
}

#[test]
fn v32_a_later_rst_replaces_an_earlier_one() {
    let mut v = validator();
    let mut spots = run(
        &transmission_events(1, &["5NN", "DE", "K5ARH", "K"], 0),
        &mut v,
    );
    spots.extend(run(
        &transmission_events(1, &["339", "DE", "K5ARH", "K"], 100_000),
        &mut v,
    ));
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].rst.as_deref(), Some("339"));
}

#[test]
fn v32_rst_survives_aging_out_of_the_word_window() {
    // 17 filler words push "5NN" out of the validator's 16-word window
    // before the spot: "most recent" is per track, not per window (D4).
    let mut v = validator();
    let mut spots = run(
        &transmission_events(1, &["5NN", "DE", "K5ARH", "K"], 0),
        &mut v,
    );
    let mut words = vec!["TEST"; 17];
    words.extend(["DE", "K5ARH", "K"]);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert_eq!(spots.len(), 1, "got {spots:?}");
    // The TEST filler classifies this spot as Cq; only `rst` is asserted.
    assert_eq!(spots[0].rst.as_deref(), Some("599"));
}

#[test]
fn v32_a_track_with_no_report_sends_none() {
    let mut v = validator();
    let spots = twice(&mut v, 1, &["DE", "K5ARH", "K"], 0);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert!(spots[0].rst.is_none());
    assert!(!spots[0].qrl_query);
}

#[test]
fn v33_spot_is_flagged_when_the_track_sent_qrl_query() {
    // Given a track's decoded text contains "QRL?"
    let mut v = validator();

    // When manta emits a spot for that track
    let spots = twice(&mut v, 1, &["QRL?", "DE", "K5ARH", "K"], 0);

    // Then the spot is flagged as a QRL? request
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].callsign, "K5ARH");
    assert!(spots[0].qrl_query);
    assert!(spots[0].rst.is_none());
}

#[test]
fn v33_a_cq_after_qrl_query_is_flagged() {
    let mut v = validator();
    let spots = twice(&mut v, 1, &["QRL?", "QRL?", "CQ", "K5ARH", "K5ARH"], 0);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].spot_type, SpotType::Cq);
    assert!(spots[0].qrl_query);
}

#[test]
fn v33_a_bare_qrl_is_not_a_query() {
    // A bare QRL is the answer ("the frequency is in use"), not the query.
    let mut v = validator();
    let spots = twice(&mut v, 1, &["QRL", "DE", "K5ARH", "K"], 0);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert!(!spots[0].qrl_query);
}

#[test]
fn v33_the_ticket_transmission_carries_both() {
    // One transmission spots here: the repeated K5ARH inside it is the
    // second message the repetition gate needs.
    let mut v = validator();
    let words = [
        "QRL?", "QRL?", "CQ", "TEST", "K5ARH", "K5ARH", "TU", "5NN", "CQ", "K5ARH",
    ];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].callsign, "K5ARH");
    assert_eq!(spots[0].spot_type, SpotType::Cq);
    assert_eq!(spots[0].rst.as_deref(), Some("599"));
    assert!(spots[0].qrl_query);
}

#[test]
fn v33_a_glyphless_placeholder_question_mark_counts() {
    // D3: the decoder's glyphless placeholder `?` (confidence exactly 0.0,
    // SPEC §4.4.4) after QRL still reads as the query.
    let mut v = validator();
    let mut ts = 0;
    for c in ['Q', 'R', 'L'] {
        v.ingest(&DecoderEvent::char_decoded(1, ts, Glyph::Char(c), 0.9));
        ts += 100;
    }
    v.ingest(&DecoderEvent::char_decoded(1, ts, Glyph::Char('?'), 0.0));
    ts += 100;
    v.ingest(&DecoderEvent::word_boundary(1, ts));
    let spots = twice(&mut v, 1, &["DE", "K5ARH", "K"], 10_000);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert!(spots[0].qrl_query);
}

#[test]
fn annotations_end_with_their_track() {
    let mut v = validator();
    let spots = run(&transmission_events(1, &["QRL?", "5NN"], 0), &mut v);
    assert!(spots.is_empty(), "got {spots:?}");
    v.ingest(&DecoderEvent::TrackClosed {
        track_id: 1,
        closure: ClosureKind::SignalEnded,
    });
    seed_meta(&mut v, 2);
    let spots = twice(&mut v, 2, &["DE", "W1AW", "K"], 200_000);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].track_id, 2);
    assert!(spots[0].rst.is_none());
    assert!(!spots[0].qrl_query);
}

#[test]
fn v33_a_merge_does_not_carry_the_losers_annotations() {
    // D5: track 2 decodes QRL? and 5NN, then merges into track 1. The
    // survivor's spot carries only what track 1 itself decoded.
    let mut v = validator();
    seed_meta(&mut v, 2);
    let spots = run(&transmission_events(2, &["QRL?", "5NN"], 0), &mut v);
    assert!(spots.is_empty(), "got {spots:?}");
    v.ingest(&DecoderEvent::TrackClosed {
        track_id: 2,
        closure: ClosureKind::Bookkeeping {
            survivor_track_id: Some(1),
        },
    });
    let spots = twice(&mut v, 1, &["DE", "K5ARH", "K"], 10_000);
    assert_eq!(spots.len(), 1, "got {spots:?}");
    assert_eq!(spots[0].track_id, 1);
    assert!(spots[0].rst.is_none());
    assert!(!spots[0].qrl_query);
}
