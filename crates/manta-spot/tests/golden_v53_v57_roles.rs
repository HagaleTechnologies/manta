//! SPEC-decode-core.md §7.1 V53 to V57 (MAN-201): a callsign is spotted as
//! the sending station only, and only once its role is settled. Event-level
//! vectors at the `Validator::ingest` contract, like V32/V33.

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::{Glyph, Prosign};
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

fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        sample_ts: 0,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
        last_char_ts: 0,
    });
}

fn close_track(v: &mut Validator, track_id: u32, closure: ClosureKind) -> Vec<Spot> {
    let mut spots = v.ingest(&DecoderEvent::SpeedUpdate {
        track_id,
        wpm: 22.0,
    });
    spots.extend(v.ingest(&DecoderEvent::TrackClosed { track_id, closure }));
    spots
}

fn validator(allow: &[&str]) -> Validator {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    v.record_decisions(true);
    for call in allow {
        v.allowlist(call);
    }
    seed_meta(&mut v, 1);
    v
}

fn calls(spots: &[Spot]) -> Vec<(&str, SpotType)> {
    spots
        .iter()
        .map(|s| (s.callsign.as_str(), s.spot_type))
        .collect()
}

#[test]
fn v53_the_addressee_of_a_call_is_never_spotted_as_its_transmitter() {
    // Terminated, twice: K1ABC at its own second boundary.
    let mut v = validator(&[]);
    let words = ["DE", "W1AW", "DE", "K1ABC", "K"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("K1ABC", SpotType::De)]);
    // DE(300) + W1AW(500) + DE(300) + K1ABC boundary at +500.
    assert_eq!(spots[0].sample_ts, 100_000 + 300 + 500 + 300 + 500);
    assert!(
        v.take_decisions().iter().all(|d| d.call != "W1AW"),
        "no decision names the station called"
    );

    // Unterminated, 1 s apart: the next `DE` makes K1ABC an addressee too.
    let mut v = validator(&[]);
    let words = ["DE", "W1AW", "DE", "K1ABC"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert!(calls(&spots).iter().all(|(c, _)| *c != "W1AW"), "{spots:?}");

    // The same pair 6.25 s apart: the silence ends the first turn.
    let mut v = validator(&[]);
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 600_000), &mut v));
    spots.extend(run(&transmission_events(1, &words, 1_200_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert!(
        calls(&spots).contains(&("K1ABC", SpotType::De)),
        "{spots:?}"
    );
    assert!(calls(&spots).iter().all(|(c, _)| *c != "W1AW"), "{spots:?}");

    // A repeated addressee.
    let mut v = validator(&[]);
    let words = ["W1AW", "W1AW", "DE", "K1ABC", "K1ABC", "K"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("K1ABC", SpotType::De)]);
}

#[test]
fn v54_a_bare_cq_with_filler_spots_as_cq() {
    let words = ["CQ", "DX", "W1AW"];
    let mut v = validator(&[]);
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(spots.is_empty(), "{spots:?}");

    let mut v = validator(&[]);
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("W1AW", SpotType::Cq)]);
    assert_eq!(
        spots[0].emission_state,
        manta_spot::EmissionState::Confirmed
    );
}

#[test]
fn v55_a_stale_cq_does_not_type_a_later_turn() {
    let mut v = validator(&[]);
    let words = [
        "CQ", "W5XYZ", "W5XYZ", "K", "5NN", "05", "TU", "73", "GL", "OM", "DE", "K1ABC", "K1ABC",
        "K",
    ];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert!(
        calls(&spots).contains(&("K1ABC", SpotType::De)),
        "{spots:?}"
    );
    assert!(
        calls(&spots).contains(&("W5XYZ", SpotType::Cq)),
        "{spots:?}"
    );
    assert!(
        !calls(&spots).contains(&("K1ABC", SpotType::Cq)),
        "{spots:?}"
    );

    let mut v = validator(&[]);
    let words = ["CQ", "W1AW", "DE", "K1ABC"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 600_000), &mut v));
    spots.extend(run(&transmission_events(1, &words, 1_200_000), &mut v));
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert!(
        calls(&spots).contains(&("K1ABC", SpotType::De)),
        "{spots:?}"
    );
    assert!(calls(&spots).iter().all(|(c, _)| *c != "W1AW"), "{spots:?}");
}

#[test]
fn v56_the_allowlist_skips_plausibility_not_role() {
    for words in [
        &["W1AW", "DE", "K1ABC", "K"][..],
        &["DE", "K1ABC", "K1ABC", "OP", "W1AW", "K"][..],
    ] {
        let mut v = validator(&["W1AW"]);
        let mut spots = run(&transmission_events(1, words, 0), &mut v);
        spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
        assert!(calls(&spots).iter().all(|(c, _)| *c != "W1AW"), "{spots:?}");
    }

    let mut v = validator(&["W1AW"]);
    let mut spots = run(&transmission_events(1, &["W1AW"], 0), &mut v);
    assert!(spots.is_empty(), "the lone call waits for its close");
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("W1AW", SpotType::Unknown)]);

    let mut v = validator(&["K5ARH"]);
    let mut spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("K5ARH", SpotType::Beacon)]);
    assert_eq!(spots[0].sample_ts, 500, "K5ARH's own boundary");
}

/// `DE K5ARH`, K5ARH allowlisted, held; K5ARH's boundary is at 800.
fn held() -> Validator {
    let mut v = validator(&["K5ARH"]);
    let spots = run(&transmission_events(1, &["DE", "K5ARH"], 0), &mut v);
    assert!(spots.is_empty(), "the newest word is unsettled: {spots:?}");
    v
}

const K5ARH_END: u64 = 800;
const SILENCE: u64 = 480_000;

fn char_at(ts: u64, glyph: Glyph) -> DecoderEvent {
    DecoderEvent::char_decoded(1, ts, glyph, 0.95)
}

fn meta_at(sample_ts: u64, last_char_ts: u64) -> DecoderEvent {
    DecoderEvent::TrackMeta {
        track_id: 1,
        sample_ts,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
        last_char_ts,
    }
}

fn assert_held_spot(spots: &[Spot], v: &mut Validator) {
    assert_eq!(calls(spots), vec![("K5ARH", SpotType::De)]);
    assert_eq!(spots[0].sample_ts, K5ARH_END);
    let spotted: Vec<_> = v
        .take_decisions()
        .into_iter()
        .filter(|d| d.spot.is_some())
        .collect();
    assert_eq!(spotted.len(), 1);
    assert_eq!(spotted[0].sample_ts, K5ARH_END);
}

#[test]
fn v57_a_call_is_decided_once_its_role_is_settled() {
    // (a) the next word.
    let mut v = held();
    let spots = run(&transmission_events(1, &["TNX"], 900), &mut v);
    assert_held_spot(&spots, &mut v);

    // (b) a `K`.
    let mut v = held();
    let spots = run(&transmission_events(1, &["K"], 900), &mut v);
    assert_held_spot(&spots, &mut v);

    // (c) an `<AR>` glyph, then a boundary.
    let mut v = held();
    let mut spots = v.ingest(&char_at(900, Glyph::Prosign(Prosign::Ar)));
    spots.extend(v.ingest(&DecoderEvent::word_boundary(1, 1_000)));
    assert_held_spot(&spots, &mut v);

    // (d) a first character 5 s later; its word's boundary decides.
    let mut v = held();
    let mut spots = v.ingest(&char_at(K5ARH_END + SILENCE, Glyph::Char('W')));
    spots.extend(v.ingest(&DecoderEvent::word_boundary(1, K5ARH_END + SILENCE + 100)));
    assert_held_spot(&spots, &mut v);

    // (e) a `TrackMeta` 5 s later with nothing in progress.
    let mut v = held();
    let spots = v.ingest(&meta_at(K5ARH_END + SILENCE, 0));
    assert_held_spot(&spots, &mut v);

    // (f), (g) the track's end.
    for closure in [
        ClosureKind::SignalEnded,
        ClosureKind::Bookkeeping {
            survivor_track_id: None,
        },
    ] {
        let mut v = held();
        let spots = close_track(&mut v, 1, closure);
        assert_held_spot(&spots, &mut v);
    }

    // (h) 4.35 s, the longest gap inside one 8 WPM transmission, settles
    // nothing.
    let mut v = held();
    let gap = (4.35 * FS) as u64;
    let spots = v.ingest(&char_at(K5ARH_END + gap, Glyph::Char('W')));
    assert!(spots.is_empty(), "{spots:?}");
    let spots = v.ingest(&meta_at(K5ARH_END + gap, K5ARH_END + gap));
    assert!(spots.is_empty(), "{spots:?}");

    // (i) an `<ERR>` right after a call retracts it.
    let mut v = validator(&["W1AW"]);
    let mut events = transmission_events(1, &["DE", "W1AX"], 0);
    events.push(char_at(900, Glyph::Prosign(Prosign::Err)));
    events.extend(transmission_events(1, &["W1AW", "K"], 1_000));
    let spots = run(&events, &mut v);
    assert_eq!(calls(&spots), vec![("W1AW", SpotType::De)]);
    assert!(v.take_decisions().iter().all(|d| d.call != "W1AX"));

    // (k) a newer spot on another track: the older held occurrence is
    // withheld `dedupe`, without moving the dedupe state back.
    let mut v = held();
    seed_meta(&mut v, 2);
    let spots = run(
        &transmission_events(2, &["DE", "K5ARH", "K"], 50_000),
        &mut v,
    );
    assert_eq!(calls(&spots), vec![("K5ARH", SpotType::De)]);
    let newer = spots[0].sample_ts;
    assert!(newer > K5ARH_END);
    v.take_decisions();
    let spots = run(&transmission_events(1, &["K"], 60_000), &mut v);
    assert!(spots.is_empty(), "{spots:?}");
    let log = v.take_decisions();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].reason, Some(manta_spot::Reason::Dedupe));
    assert_eq!(log[0].sample_ts, K5ARH_END);

    // (l) a settled mention stays one after its framing scrolls out.
    let mut v = validator(&["W1AW"]);
    let mut words = vec!["DE", "K1ABC", "OP", "W1AW", "K"];
    words.extend(["TNX"; 16]);
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert!(calls(&spots).iter().all(|(c, _)| *c != "W1AW"), "{spots:?}");
    assert!(v.take_decisions().iter().all(|d| d.call != "W1AW"));
}
