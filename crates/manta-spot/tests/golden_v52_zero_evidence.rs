//! SPEC-decode-core.md §7.1 V52 (MAN-106): the zero-evidence abstention
//! gate and the emission states. A `CQ DE W1AW W1AW K` event sequence
//! whose every `CharDecoded` carries confidence 0.0 must produce NO
//! public spot and a withheld `noEvidence` decision — reproduced from
//! Tony Hagale's measured emission at conf=1.028e-7, where the raw
//! composition (f32::EPSILON geo-mean × 0.75 × 1.15 SCP) still published.
//! The gate is on evidence, not on a score threshold: no arbitrary high
//! cutoff, per investigation 11.

use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use manta_spot::{Decision, EmissionState, Reason, Spot, SpotType, Validator, Verdict};

const FS: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

/// `word_events` with a per-character confidence parameter: the event
/// contract the decoder emits (`char_decoded(..., confidence)`), not the
/// audio behind it — exactly where the zero-evidence state is pinned.
fn word_events_at(
    track_id: u32,
    text: &str,
    start_ts: u64,
    confidence: f32,
) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for c in text.chars() {
        events.push(DecoderEvent::char_decoded(
            track_id,
            ts,
            Glyph::Char(c),
            confidence,
        ));
        ts += 100;
    }
    events.push(DecoderEvent::word_boundary(track_id, ts));
    ts += 100;
    (events, ts)
}

fn transmission_events_at(
    track_id: u32,
    words: &[&str],
    start_ts: u64,
    confidence: f32,
) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for word in words {
        let (mut w_events, next_ts) = word_events_at(track_id, word, ts, confidence);
        events.append(&mut w_events);
        ts = next_ts;
    }
    events
}

/// A word whose characters split between zero and positive confidence —
/// the mixed-`?` case (research §4): calibration handles it, the gate
/// must not.
fn mixed_word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for (i, c) in text.chars().enumerate() {
        let confidence = if i % 2 == 0 { 0.0 } else { 0.4 };
        events.push(DecoderEvent::char_decoded(
            track_id,
            ts,
            Glyph::Char(c),
            confidence,
        ));
        ts += 100;
    }
    events.push(DecoderEvent::word_boundary(track_id, ts));
    ts += 100;
    (events, ts)
}

fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        sample_ts: 0,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
    });
}

fn cq_words() -> [&'static str; 6] {
    ["CQ", "CQ", "DE", "W1AW", "W1AW", "K"]
}

#[test]
fn v52_zero_evidence_cq_never_emits_and_records_no_evidence() {
    for scp in [None, Some("W1AW\n")] {
        let mut v = Validator::new(FS, CTY_FIXTURE, scp);
        v.record_decisions(true);
        seed_meta(&mut v, 1);
        let events = transmission_events_at(1, &cq_words(), 0, 0.0);
        let spots: Vec<Spot> = events.iter().flat_map(|e| v.ingest(e)).collect();
        assert!(
            spots.is_empty(),
            "an all-zero-confidence candidate must abstain (scp={}), got {spots:?}",
            scp.is_some()
        );
        let decisions = v.take_decisions();
        let no_evidence: Vec<&Decision> = decisions
            .iter()
            .filter(|d| d.reason == Some(Reason::NoEvidence))
            .collect();
        assert!(
            !no_evidence.is_empty(),
            "the abstention must be recorded as a NoEvidence decision, got {decisions:?}"
        );
        assert!(no_evidence.iter().all(|d| d.verdict == Verdict::Withheld));
        assert!(
            no_evidence.iter().all(|d| d.call == "W1AW"),
            "the gated candidate is W1AW: {no_evidence:?}"
        );
    }
}

#[test]
fn v52_positive_evidence_still_spots_and_mixed_words_still_spot() {
    // The gate does not fire on positive evidence.
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let mut spots: Vec<Spot> = transmission_events_at(1, &cq_words(), 0, 0.95)
        .iter()
        .flat_map(|e| v.ingest(e))
        .collect();
    spots.extend(
        transmission_events_at(1, &cq_words(), 100_000, 0.95)
            .iter()
            .flat_map(|e| v.ingest(e)),
    );
    assert_eq!(spots.len(), 1, "normal evidence must still spot: {spots:?}");

    // Mixed zero/positive characters: the word HAS acoustic evidence, so
    // it is calibration's problem (near-zero probability, `tentative`),
    // not the gate's.
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let mut events = Vec::new();
    let mut ts = 0u64;
    for (i, word) in cq_words().iter().enumerate() {
        let (mut w, next) = if i == 3 || i == 4 {
            mixed_word_events(1, word, ts)
        } else {
            word_events_at(1, word, ts, 0.6)
        };
        events.append(&mut w);
        ts = next;
    }
    let mut spots: Vec<Spot> = events.iter().flat_map(|e| v.ingest(e)).collect();
    spots.extend(
        transmission_events_at(1, &cq_words(), 100_000, 0.6)
            .iter()
            .flat_map(|e| v.ingest(e)),
    );
    assert_eq!(
        spots.len(),
        1,
        "a mixed-? word still spots (calibration's case), got {spots:?}"
    );
}

#[test]
fn v52_two_repetition_cq_spot_is_confirmed() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let mut spots: Vec<Spot> = transmission_events_at(1, &cq_words(), 0, 0.95)
        .iter()
        .flat_map(|e| v.ingest(e))
        .collect();
    spots.extend(
        transmission_events_at(1, &cq_words(), 100_000, 0.95)
            .iter()
            .flat_map(|e| v.ingest(e)),
    );
    let spot = &spots[0];
    assert_eq!(spot.spot_type, SpotType::Cq);
    assert_eq!(spot.emission_state, EmissionState::Confirmed);
    assert!(spot.confidence >= 0.5, "confirmed implies the threshold");
}

#[test]
fn v52_single_repetition_allowlisted_spot_is_tentative() {
    // The Phase-4 rule: `confirmed` iff the calibrated probability
    // reaches PUBLISH_CONFIRMED_THRESHOLD. An allowlisted single decode
    // with degraded characters (0.3 per character) calibrates far below
    // the threshold, so it publishes `tentative`.
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    v.allowlist("W1AW");
    seed_meta(&mut v, 1);
    let spots: Vec<Spot> = transmission_events_at(1, &cq_words(), 0, 0.3)
        .iter()
        .flat_map(|e| v.ingest(e))
        .collect();
    assert_eq!(spots.len(), 1, "allowlisted spots on one decode: {spots:?}");
    assert_eq!(spots[0].emission_state, EmissionState::Tentative);
    assert!(spots[0].confidence < 0.5);
    // And the state is exactly the threshold rule on the spot's own
    // calibrated probability.
    assert_eq!(
        spots[0].emission_state,
        if spots[0].confidence >= manta_spot::confidence::calibration::PUBLISH_CONFIRMED_THRESHOLD {
            EmissionState::Confirmed
        } else {
            EmissionState::Tentative
        }
    );
}

#[test]
fn v52_emission_state_serializes_camel_case_and_documented_labels() {
    let base = Spot {
        callsign: "W1AW".into(),
        freq_hz: 14_000_000.0,
        snr_db: 11.0,
        wpm: 20.0,
        spot_type: SpotType::Cq,
        confidence: 0.71,
        emission_state: EmissionState::Confirmed,
        track_id: 7,
        sample_ts: 4_123_456,
        rst: None,
        qrl_query: false,
    };
    let json = serde_json::to_string(&base).unwrap();
    assert!(
        json.contains(r#""emissionState":"confirmed""#),
        "field serializes as emissionState with the documented label: {json}"
    );
    let tentative = Spot {
        emission_state: EmissionState::Tentative,
        ..base
    };
    let json = serde_json::to_string(&tentative).unwrap();
    assert!(json.contains(r#""emissionState":"tentative""#), "{json}");
}
