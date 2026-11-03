//! SPEC-decode-core.md §7.1 V58 (MAN-201): a trailing `T` alone is not
//! beacon evidence. A `<call> T` spots as `BEACON` only for an allowlisted
//! call; `V V V <call>` keeps its deferred, repetition-exempt path (V18).

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::{Decision, Reason, Spot, SpotType, Validator};

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

fn no_evidence(log: &[Decision]) -> Vec<&Decision> {
    log.iter()
        .filter(|d| d.reason == Some(Reason::BeaconNoEvidence))
        .collect()
}

#[test]
fn v58_a_trailing_t_alone_is_not_beacon_evidence() {
    let mut v = validator(&[]);
    let mut spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert!(spots.is_empty(), "{spots:?}");
    let log = v.take_decisions();
    let d = no_evidence(&log);
    assert_eq!(d.len(), 1, "{log:?}");
    assert_eq!(d[0].call, "K5ARH");
    assert_eq!(d[0].reps, None);
    assert_eq!(d[0].spot_type, SpotType::Beacon);
    assert_eq!(v.suppression_counts().beacon_no_evidence, 1);

    for words in [
        &["K1ABC", "K1ABC", "T"][..],
        &["W5XYZ", "5NN", "05", "K1ABC", "T"][..],
    ] {
        let mut v = validator(&[]);
        let mut spots = run(&transmission_events(1, words, 0), &mut v);
        spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
        assert!(spots.is_empty(), "{words:?}: {spots:?}");
    }

    // One occurrence counts once across later boundaries.
    let mut v = validator(&[]);
    let words = ["K5ARH", "T", "TNX", "73", "GL"];
    run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(v.suppression_counts().beacon_no_evidence, 1);

    // A guarded shape records neither reason for an unlisted call.
    let mut v = validator(&[]);
    run(
        &transmission_events(1, &["CQ", "DX", "K5ARH", "T"], 0),
        &mut v,
    );
    let log = v.take_decisions();
    assert!(no_evidence(&log).is_empty(), "{log:?}");
    assert_eq!(v.suppression_counts().power_step_guard, 0);

    // A listed call spots, with no such decision.
    let mut v = validator(&["K5ARH"]);
    let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
    assert_eq!(calls(&spots), vec![("K5ARH", SpotType::Beacon)]);
    let log = v.take_decisions();
    assert!(no_evidence(&log).is_empty(), "{log:?}");

    // `V V V` keeps its deferred path.
    let mut v = validator(&[]);
    let mut spots = run(
        &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
        &mut v,
    );
    spots.extend(close_track(&mut v, 1, ClosureKind::SignalEnded));
    assert_eq!(calls(&spots), vec![("K5ARH", SpotType::Beacon)]);

    // A call failing grammar or cty has nothing to explain.
    for words in [&["E", "T"][..], &["QQ9ZZZ", "T"][..]] {
        let mut v = validator(&[]);
        run(&transmission_events(1, words, 0), &mut v);
        let log = v.take_decisions();
        assert!(no_evidence(&log).is_empty(), "{words:?}: {log:?}");
    }
}
