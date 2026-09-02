//! SPEC-decode-core.md §7.1 V11-V17: manta-spot validator vectors.

use manta_decode::events::DecoderEvent;
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

#[test]
fn v11_context_parse_sets_spot_type() {
    let cases: &[(&[&str], SpotType)] = &[
        (&["CQ", "CQ", "DE", "K5ARH", "K5ARH", "K"], SpotType::Cq),
        (&["DE", "K5ARH", "K"], SpotType::De),
        (&["CQ", "TEST", "K5ARH", "K5ARH"], SpotType::Cq),
        (&["K5ARH", "UP", "UP"], SpotType::De),
        (&["V", "V", "V", "K5ARH", "K5ARH"], SpotType::Beacon),
    ];
    for (words, expected_type) in cases {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let mut spots = run(&transmission_events(1, words, 0), &mut v);
        spots.extend(run(&transmission_events(1, words, 100_000), &mut v));
        let hit = spots
            .iter()
            .find(|s| s.callsign == "K5ARH")
            .unwrap_or_else(|| panic!("no K5ARH spot for words {words:?}, got {spots:?}"));
        assert_eq!(hit.spot_type, *expected_type, "words {words:?}");
    }
}

#[test]
fn v12_bogus_prefix_rejected() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "ZZ9ZZZ", "K"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert!(
        spots.is_empty(),
        "bogus-prefix callsign must never be spotted, got {spots:?}"
    );
}

#[test]
fn v13_scp_membership_boosts_confidence_without_gating_absence() {
    let scp_fixture = "K5ARH\n";
    let words = ["DE", "K5ARH", "K"];
    let words_not_in_scp = ["DE", "K9ABC", "K"];

    let run_twice = |v: &mut Validator, words: &[&str]| -> Vec<Spot> {
        let mut spots = run(&transmission_events(1, words, 0), v);
        spots.extend(run(&transmission_events(1, words, 100_000), v));
        spots
    };

    let mut v_scp = Validator::new(FS, CTY_FIXTURE, Some(scp_fixture));
    let mut v_noscp = Validator::new(FS, CTY_FIXTURE, None);
    let with_scp = run_twice(&mut v_scp, &words);
    let without_scp = run_twice(&mut v_noscp, &words);
    assert!(!with_scp.is_empty() && !without_scp.is_empty());
    assert!(
        with_scp[0].confidence > without_scp[0].confidence,
        "SCP membership must raise confidence: {} vs {}",
        with_scp[0].confidence,
        without_scp[0].confidence
    );

    let mut v_absent = Validator::new(FS, CTY_FIXTURE, Some(scp_fixture));
    let no_member = run_twice(&mut v_absent, &words_not_in_scp);
    assert!(
        !no_member.is_empty(),
        "SCP absence must not gate a structurally-valid, cty-allocated call"
    );
}

#[test]
fn v14_repetition_gate_requires_two_reps() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "K5ARH", "K"];
    let once = run(&transmission_events(1, &words, 0), &mut v);
    assert!(once.is_empty(), "1 rep must never spot, got {once:?}");

    let twice = run(&transmission_events(1, &words, 100_000), &mut v);
    assert!(!twice.is_empty(), "2 reps must spot");
}

#[test]
fn v15_dedupe_suppresses_then_allows_on_snr_jump() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "K5ARH", "K"];

    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert_eq!(
        spots.len(),
        1,
        "must spot exactly once so far, got {spots:?}"
    );

    let suppressed = run(&transmission_events(1, &words, 200_000), &mut v);
    assert!(
        suppressed.is_empty(),
        "re-spot inside the suppression window with no SNR/type change must be suppressed"
    );

    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 6.0,
        freq_hz: 0.0,
    });
    let allowed = run(&transmission_events(1, &words, 300_000), &mut v);
    assert!(
        !allowed.is_empty(),
        "an SNR jump >= 6 dB must override dedupe suppression"
    );
}

#[test]
fn v16_beacon_pattern_exempt_from_repetition_gate() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["V", "V", "V", "K5ARH"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(spots.len(), 1, "a BEACON-tagged spot must emit on the first decode");
    assert_eq!(spots[0].callsign, "K5ARH");
    assert_eq!(spots[0].spot_type, SpotType::Beacon);
}

#[test]
fn v17_allowlisted_call_bypasses_validation_and_repetition() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    v.allowlist("ZZ9ZZZ");
    let words = ["DE", "ZZ9ZZZ", "K"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(
        spots.len(),
        1,
        "an allowlisted call must spot despite a bogus cty prefix and a single decode"
    );
    assert_eq!(spots[0].callsign, "ZZ9ZZZ");
}
