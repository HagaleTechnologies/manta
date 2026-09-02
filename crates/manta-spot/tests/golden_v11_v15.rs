//! SPEC-decode-core.md §7.1 V11-V15, V18-V30: manta-spot validator
//! vectors. (V16-V17, MAN-31's operator suppression vectors, live in
//! golden_v16_v17.rs.)

use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use manta_spot::{Blocklist, Spot, SpotType, Validator};

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

/// Real telemetry, so `try_spot`'s `has_meta` gate (MAN-28 round 8) doesn't
/// hold back every spot in tests that don't otherwise care about metadata
/// timing.
fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
    });
}

#[test]
fn v11_context_parse_sets_spot_type() {
    let cases: &[(&[&str], SpotType)] = &[
        (&["CQ", "CQ", "DE", "K5ARH", "K5ARH", "K"], SpotType::Cq),
        (&["DE", "K5ARH", "K"], SpotType::De),
        (&["CQ", "TEST", "K5ARH", "K5ARH"], SpotType::Cq),
        (&["K5ARH", "UP", "UP"], SpotType::De),
        (&["V", "V", "V", "K5ARH", "K5ARH"], SpotType::Beacon),
        (&["K5ARH", "T"], SpotType::Beacon),
    ];
    for (words, expected_type) in cases {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
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
    seed_meta(&mut v_scp, 1);
    let mut v_noscp = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v_noscp, 1);
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
    seed_meta(&mut v_absent, 1);
    let no_member = run_twice(&mut v_absent, &words_not_in_scp);
    assert!(
        !no_member.is_empty(),
        "SCP absence must not gate a structurally-valid, cty-allocated call"
    );
}

#[test]
fn v14_repetition_gate_requires_two_reps() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["DE", "K5ARH", "K"];
    let once = run(&transmission_events(1, &words, 0), &mut v);
    assert!(once.is_empty(), "1 rep must never spot, got {once:?}");

    let twice = run(&transmission_events(1, &words, 100_000), &mut v);
    assert!(!twice.is_empty(), "2 reps must spot");
}

#[test]
fn v15_dedupe_suppresses_then_allows_on_snr_jump() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    // Real telemetry, but a low starting SNR -- the test's own later
    // TrackMeta jumps to 6.0 dB, which must read as a genuine >= 6 dB
    // increase for the dedupe override below.
    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 0.0,
        freq_hz: 14_000_000.0,
    });
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
        freq_hz: 14_000_000.0,
    });
    let allowed = run(&transmission_events(1, &words, 300_000), &mut v);
    assert!(
        !allowed.is_empty(),
        "an SNR jump >= 6 dB must override dedupe suppression"
    );
}

#[test]
fn v18_beacon_pattern_exempt_from_repetition_gate() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["V", "V", "V", "K5ARH"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(
        spots.len(),
        1,
        "a BEACON-tagged spot must emit on the first decode"
    );
    assert_eq!(spots[0].callsign, "K5ARH");
    assert_eq!(spots[0].spot_type, SpotType::Beacon);
}

#[test]
fn v19_allowlisted_call_bypasses_validation_and_repetition() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
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

/// MAN-28/MAN-31 interaction: an explicit blocklist entry is the more
/// specific, deliberate operator override and must not be silently
/// defeated by a broader allowlist entry on the same callsign.
#[test]
fn blocklisted_callsign_is_never_spotted_even_if_also_allowlisted() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_blocklist(Blocklist::parse("K5ARH\n"));
    seed_meta(&mut v, 1);
    v.allowlist("K5ARH");
    let words = ["DE", "K5ARH", "K"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        spots.is_empty(),
        "a blocklisted callsign must never spot, even if also allowlisted, got {spots:?}"
    );
}

/// V20: an allowlisted callsign with no recognized CQ/DE/UP/beacon context
/// pattern -- the primary real-world Watch List scenario (an NCDXF beacon
/// transmits its callsign followed by power-step dashes, no framing
/// words at all) -- must still spot. `context::parse` returns no matches
/// at all for a standalone callsign; the allowlist bypass must not
/// require a pattern match at all.
#[test]
fn v20_allowlisted_call_spots_with_no_context_pattern() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("QQ9ZZZ");
    let words = ["QQ9ZZZ"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(
        spots.len(),
        1,
        "an allowlisted call with no CQ/DE/UP/beacon pattern must still spot, got {spots:?}"
    );
    assert_eq!(spots[0].callsign, "QQ9ZZZ");
    assert_eq!(spots[0].spot_type, SpotType::Unknown);
}

/// V21: an allowlisted word must be found independently of context
/// parsing, not merely as a fallback when parsing finds nothing at all.
/// A stale, already-attempted context match elsewhere in the 16-word
/// window (here: "CQ K5ARH", attempted at an earlier word boundary, never
/// spotted since it decoded only once) must not block discovery of a
/// different, freshly-allowlisted word that arrives afterward in the same
/// window.
#[test]
fn v21_allowlisted_word_found_despite_a_stale_attempted_context_match() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("QQ9ZZZ");
    let words = ["CQ", "K5ARH", "QQ9ZZZ"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        spots.iter().any(|s| s.callsign == "QQ9ZZZ"),
        "an allowlisted word must be found even when an unrelated, \
         already-attempted context match (K5ARH) exists in the window, got {spots:?}"
    );
}

/// V22: a fast, first-decode-exempt spot (BEACON or allowlist) must not be
/// emitted before the track's first `TrackMeta` event -- `TrackState`'s
/// `freq_hz`/`snr_db` still hold their `0.0` defaults until then, and the
/// old repetition gate only *incidentally* hid this by taking long enough
/// that real telemetry always arrived first.
#[test]
fn v22_exempt_spot_waits_for_track_metadata_before_emitting() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    v.allowlist("QQ9ZZZ");
    let words = ["QQ9ZZZ"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        spots.is_empty(),
        "must not spot with bogus 0 Hz/0 dB telemetry before TrackMeta arrives, got {spots:?}"
    );

    // The pending candidate is retried the moment metadata arrives (V25),
    // so the spot comes out of this same ingest call.
    let more_spots = v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 15.0,
        freq_hz: 14_020_000.0,
    });
    assert!(
        more_spots
            .iter()
            .any(|s| s.callsign == "QQ9ZZZ" && s.freq_hz == 14_020_000.0),
        "once metadata arrives, the pending candidate must spot with real telemetry, got {more_spots:?}"
    );
}

/// V23: an allowlisted word spotted immediately with no context yet
/// (type `Unknown`) must be reclassified when a trailing word completes a
/// real context pattern afterward -- "K5ARH" alone spots as `Unknown`,
/// then "UP" arrives completing `<call> UP` (type `De`). The correction
/// must not be permanently blocked by the word already being attempted;
/// dedupe's existing type-changed override lets the corrected spot
/// through.
#[test]
fn v23_allowlisted_call_is_reclassified_when_trailing_context_completes() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("K5ARH");

    let first = run(&transmission_events(1, &["K5ARH"], 0), &mut v);
    assert_eq!(
        first.len(),
        1,
        "the allowlisted call must spot immediately with no context, got {first:?}"
    );
    assert_eq!(first[0].spot_type, SpotType::Unknown);

    let second = run(&transmission_events(1, &["UP"], 300_000), &mut v);
    assert!(
        second
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::De),
        "K5ARH UP completing afterward must produce a reclassified De spot, got {second:?}"
    );
}

/// V24: a reclassification must reuse the word's existing repetition
/// count, not record another decode -- otherwise an ordinary,
/// non-exempt callsign decoded only once could spot after a
/// reclassification alone inflates its rep count to 2. "DE K5ARH"
/// decodes once (type De, reps=1, held back by the repetition gate);
/// a later "CQ" token then reclassifies the same word to type Cq. That
/// reclassification must NOT itself count as a second decode.
#[test]
fn v24_reclassification_does_not_inflate_the_repetition_count() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["DE", "K5ARH", "CQ"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        spots.is_empty(),
        "K5ARH decoded only once must not spot even though its type was \
         reclassified from De to Cq by the trailing CQ token, got {spots:?}"
    );
}

/// V25: a pending first-decode-exempt candidate must be retried the
/// moment `TrackMeta` arrives, not merely on the next `WordBoundary`. A
/// short transmission with no further words after the one that completed
/// the exemption would otherwise lose it permanently -- `try_spot` is
/// only ever invoked by a word boundary, so if none follows, a pending
/// candidate held back by V22's metadata gate would never be evaluated
/// again.
#[test]
fn v25_pending_candidate_retried_when_metadata_arrives_with_no_further_words() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    v.allowlist("QQ9ZZZ");
    let words = ["QQ9ZZZ"];
    let none_yet = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        none_yet.is_empty(),
        "no TrackMeta yet, must not spot with bogus telemetry, got {none_yet:?}"
    );

    let spots = v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 15.0,
        freq_hz: 14_020_000.0,
    });
    assert!(
        spots.iter().any(|s| s.callsign == "QQ9ZZZ"),
        "the pending exempt candidate must spot as soon as metadata arrives, \
         even with no further words, got {spots:?}"
    );
}

/// V26: reclassification must only ever promote a word's type (e.g.
/// `Unknown` -> a real context type, V23), never downgrade an
/// already-contextualized word back to `Unknown` just because its framing
/// word aged out of the 16-word window. "DE K5ARH" spots as `De`; once
/// 15 more words push "DE" out of the window while "K5ARH" remains, the
/// allowlist fallback would otherwise reclassify it to `Unknown` and
/// dedupe's type-changed override would wrongly re-emit it.
#[test]
fn v26_reclassification_never_downgrades_a_contextual_type_to_unknown() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("K5ARH");

    let first = run(&transmission_events(1, &["DE", "K5ARH"], 0), &mut v);
    assert_eq!(first.len(), 1, "got {first:?}");
    assert_eq!(first[0].spot_type, SpotType::De);

    // Push 15 more words so "DE" ages out of the 16-word window while
    // "K5ARH" remains.
    let filler: Vec<String> = (1..=15).map(|i| format!("QQQ{i}")).collect();
    let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
    let spots = run(&transmission_events(1, &filler_refs, 300_000), &mut v);

    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Unknown),
        "K5ARH must not revert to Unknown just because its framing word \
         aged out of the window, got {spots:?}"
    );
}

/// V27: reclassification must never downgrade BETWEEN two contextual
/// types either, not just to/from `Unknown` (V26) -- the same aging-out
/// bug shape recurs for any pair of pattern types. "CQ DE K5ARH" spots as
/// `Cq` (DE_RE matches, and a bare CQ token is present in the window);
/// once 15 more words push both "CQ" and "DE" out while "K5ARH" remains,
/// the window re-scan would otherwise reclassify it to `De`.
#[test]
fn v27_reclassification_never_downgrades_between_contextual_types_via_aging() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("K5ARH");

    let first = run(&transmission_events(1, &["CQ", "DE", "K5ARH"], 0), &mut v);
    assert_eq!(first.len(), 1, "got {first:?}");
    assert_eq!(first[0].spot_type, SpotType::Cq);

    // Push enough filler words so both "CQ" and "DE" age out of the
    // 16-word window while "K5ARH" remains.
    let filler: Vec<String> = (1..=15).map(|i| format!("QQQ{i}")).collect();
    let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
    let spots = run(&transmission_events(1, &filler_refs, 300_000), &mut v);

    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::De),
        "K5ARH must not be reclassified from Cq to De just because CQ/DE \
         aged out of the window, got {spots:?}"
    );
}

/// V28: the flip side of V26/V27 -- a genuine reclassification driven by a
/// newly-arrived trailing word must still be accepted, not just rejected
/// wholesale. "DE K5ARH" spots as `De`; a `CQ` token arriving immediately
/// afterward (not via window aging) is real new information and must
/// promote the spot to `Cq`.
#[test]
fn v28_reclassification_still_accepted_when_driven_by_a_new_word() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    v.allowlist("K5ARH");

    let first = run(&transmission_events(1, &["DE", "K5ARH"], 0), &mut v);
    assert_eq!(first.len(), 1, "got {first:?}");
    assert_eq!(first[0].spot_type, SpotType::De);

    let second = run(&transmission_events(1, &["CQ"], 300_000), &mut v);
    assert!(
        second
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Cq),
        "a genuinely new trailing CQ token must still promote De to Cq, got {second:?}"
    );
}

/// V29: provenance must bind to the exact `Word` occurrence being
/// evaluated, not whichever occurrence `context::parse`'s regex happened
/// to match first. "CQ DE K5ARH DE K5ARH" repeats "DE K5ARH" -- the
/// second, newest K5ARH is the one `evaluate_candidate` selects (its
/// word-lookup always picks the newest matching word), but
/// `context::parse`'s first-match regex describes the FIRST "DE K5ARH"
/// occurrence's span. If that mismatch stores the wrong (lower)
/// `classified_max_seq` on the newest word, then once "CQ" (and the first
/// "DE") age out, the remaining "DE K5ARH" looks like new context even
/// though it produced the original spot -- reproducing V27's bug shape
/// through a normal (non-allowlisted) repeated call.
#[test]
fn v29_provenance_bound_to_exact_word_occurrence_across_repetitions() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);

    let words = ["CQ", "DE", "K5ARH", "DE", "K5ARH"];
    let first = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        first
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Cq),
        "K5ARH must spot as Cq after 2 reps, got {first:?}"
    );

    // Push enough filler so "CQ" and the first "DE" age out of the
    // 16-word window while the second "DE K5ARH" occurrence remains.
    let filler: Vec<String> = (1..=13).map(|i| format!("QQQ{i}")).collect();
    let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
    let spots = run(&transmission_events(1, &filler_refs, 300_000), &mut v);

    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::De),
        "K5ARH must not be reclassified to De just because CQ aged out, \
         even across a repeated DE K5ARH occurrence, got {spots:?}"
    );
}

/// MAN-37: the same first-decode repetition-gate exemption V18 proves for
/// the textual `V V V <call>` beacon preamble, extended to the power-step
/// pattern (`<call> T`) that real NCDXF/IARU beacons actually decode as.
#[test]
fn v30_power_step_beacon_pattern_exempt_from_repetition_gate() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["K5ARH", "T"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert_eq!(
        spots.len(),
        1,
        "a power-step BEACON-tagged spot must emit on the first decode"
    );
    assert_eq!(spots[0].callsign, "K5ARH");
    assert_eq!(spots[0].spot_type, SpotType::Beacon);
}

/// MAN-37 (Codex review rounds 3-6): after three consecutive rounds of
/// position/range-based refinements to the CQ/DE-framing guard each
/// traded one false-positive shape for another (see `context::parse`'s
/// own docs for the full history), the guard was deliberately simplified
/// to a coarse, whole-window rule: a bare `CQ` or `DE` token ANYWHERE in
/// the window suppresses the power-step fallback entirely, full stop. A
/// named-pattern match ("DE W1AW") and a power-step candidate on a
/// DIFFERENT callsign ("K5ARH T") are genuinely independent transmission
/// fragments, but under the coarse rule the bare "DE" still suppresses
/// K5ARH's candidate -- W1AW still spots normally on its own.
#[test]
fn a_resolved_named_match_still_suppresses_an_unrelated_power_step_candidate() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["DE", "W1AW", "K5ARH", "T"];
    // De isn't exempt from the repetition gate (only Beacon is) -- decode
    // twice so W1AW gets its own chance to spot normally.
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert!(
        spots
            .iter()
            .any(|s| s.callsign == "W1AW" && s.spot_type == SpotType::De),
        "W1AW must still spot as De, got {spots:?}"
    );
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon -- the bare DE elsewhere in the \
         window suppresses the power-step fallback entirely under the \
         coarse guard, got {spots:?}"
    );
}

/// MAN-37: when a named match and the power-step fallback would otherwise
/// name the SAME callsign, e.g. a normal double-call CQ ("CQ K5ARH
/// K5ARH") that also decodes a trailing "T", the coarse CQ/DE guard
/// suppresses the power-step candidate outright (the bare "CQ" is
/// present) -- K5ARH spots once, as `Cq`, with no Beacon reclassification.
#[test]
fn cq_call_with_trailing_t_spots_once_as_cq_not_beacon() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["CQ", "K5ARH", "K5ARH", "T"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Cq),
        "K5ARH must spot as Cq, got {spots:?}"
    );
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not also reclassify to Beacon -- the bare CQ present \
         in the window suppresses the power-step fallback entirely under \
         the coarse guard, got {spots:?}"
    );
}

/// MAN-37 (Codex review round 3's original motivation, deliberately
/// reverted along with the rest of the position-based guard -- see
/// `context::parse`'s own docs): an unresolved "CQ DX" earlier in the
/// window now suppresses a later, unrelated power-step beacon occurrence
/// too, even several words on in the same 16-word window. A real NCDXF
/// beacon repeats every ~10s, so an occasional suppression here is an
/// accepted trade for a much simpler, more robust rule.
#[test]
fn a_stale_unresolved_cq_several_words_earlier_still_suppresses_a_later_beacon() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["CQ", "DX", "FILLER1", "FILLER2", "FILLER3", "K5ARH", "T"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon -- the bare CQ elsewhere in the \
         window suppresses the power-step fallback entirely under the \
         coarse guard, got {spots:?}"
    );
}

/// MAN-37 (Codex review round 4): two distinct power-step IDs decoding
/// before the track's first TrackMeta (V22's `has_meta` gate, MAN-28 round
/// 8) means NEITHER got a chance to be attempted before both arrived --
/// collapsing to a single "best" match here would permanently lose
/// whichever one wasn't kept. Both W1AW and K5ARH must spot once metadata
/// arrives and the pending candidates are retried (V25).
#[test]
fn power_step_beacon_retains_every_unattempted_occurrence_across_the_metadata_gate() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["W1AW", "T", "K5ARH", "T"];
    let none_yet = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        none_yet.is_empty(),
        "no TrackMeta yet, must not spot with bogus telemetry, got {none_yet:?}"
    );

    let spots = v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 15.0,
        freq_hz: 14_020_000.0,
    });
    assert!(
        spots
            .iter()
            .any(|s| s.callsign == "W1AW" && s.spot_type == SpotType::Beacon),
        "W1AW must spot as Beacon once metadata arrives, got {spots:?}"
    );
    assert!(
        spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must also spot as Beacon -- neither occurrence was ever \
         attempted before metadata arrived, so neither may be dropped, \
         got {spots:?}"
    );
}

/// MAN-37: a real CQ preamble commonly repeats the callsign ("CQ DX
/// K5ARH K5ARH") -- under the coarse whole-window guard (see
/// `context::parse`'s own docs) the bare "CQ" suppresses the power-step
/// fallback regardless of how many words sit between it and the
/// candidate, so this repeated-call shape is blocked the same as any
/// other.
#[test]
fn power_step_beacon_blocked_by_unresolved_cq_with_a_repeated_call_preamble() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["CQ", "DX", "K5ARH", "K5ARH", "T"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon -- the repeated-call CQ preamble \
         is still unresolved framing, got {spots:?}"
    );
}

/// MAN-37 (Codex review round 6): two SEPARATE resolved DE fragments
/// ("DE W1AW" and "DE N1AA") preceding a power-step candidate used to
/// break the range-containment version of this guard (only the first
/// named match's range was ever tracked, so the second fragment looked
/// unresolved). Under the coarse whole-window rule the point is moot --
/// any bare DE at all suppresses the fallback, so K5ARH is blocked
/// regardless of how many resolved fragments came before it.
#[test]
fn power_step_beacon_blocked_despite_multiple_resolved_de_fragments() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["DE", "W1AW", "DE", "N1AA", "K5ARH", "T"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon -- bare DE elsewhere in the window \
         suppresses the power-step fallback entirely, got {spots:?}"
    );
}

/// MAN-37 (Codex review round 6): unresolved framing before the candidate
/// and a resolved named match AFTER it used to bridge over the candidate
/// itself in the range-containment version of this guard (the DE/CQ
/// disambiguation range spans from the earliest bare CQ to the DE match,
/// however far apart). Under the coarse whole-window rule the ordering is
/// irrelevant -- any bare CQ/DE anywhere blocks the fallback.
#[test]
fn power_step_beacon_blocked_when_unresolved_cq_precedes_a_later_resolved_de() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["CQ", "DX", "K5ARH", "T", "DE", "W1AW"];
    let spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon -- bare CQ/DE elsewhere in the \
         window suppresses the power-step fallback entirely, got {spots:?}"
    );
}

/// MAN-37 (Codex review round 7): the CQ/DE guard is recomputed solely
/// from the current window, so a withheld candidate that never reaches
/// `Validator` never marks its word `attempted` -- once the CQ/DE token
/// that triggered the guard ages out of the 16-word window, the SAME
/// occurrence (no newer evidence, nothing new decoded) would otherwise
/// pass the guard and spot as if freshly seen. `Validator` must burn a
/// suppressed candidate's word itself so the suppression survives.
#[test]
fn power_step_beacon_suppression_survives_the_triggering_cq_aging_out() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    let words = ["CQ", "K5ARH", "T"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot as Beacon while CQ is still in the window, \
         got {spots:?}"
    );

    // Push 14 more words so CQ (and only CQ) ages out of the 16-word
    // window, leaving K5ARH and T still present with no new evidence.
    let filler: Vec<String> = (1..=14).map(|i| format!("QQQ{i}")).collect();
    let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
    spots.extend(run(&transmission_events(1, &filler_refs, 100_000), &mut v));

    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must still not spot as Beacon once CQ ages out -- no new \
         evidence arrived, so the original suppression must hold, got \
         {spots:?}"
    );
}

/// MAN-37 (Codex review round 9): a power-step match must bind to the
/// EXACT decoded word the regex captured, not whichever word currently
/// shares its text. "CQ DX K5ARH T" suppresses and burns the original
/// K5ARH occurrence; once exactly enough filler words evict CQ (and only
/// CQ) at the same moment a brand-new, textually-identical but otherwise
/// unrelated standalone "K5ARH" (no trailing T at all) decodes, a
/// text-based lookup would incorrectly resolve the old "K5ARH T" match to
/// this new, never-attempted word and spot it as Beacon despite it never
/// having any power-step framing of its own.
#[test]
fn power_step_beacon_binds_to_the_captured_word_not_a_same_text_newer_one() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);

    // CQ, DX, K5ARH, T + 12 filler = 16 words: CQ is still the oldest,
    // still in the window.
    let words = ["CQ", "DX", "K5ARH", "T"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    let filler: Vec<String> = (1..=12).map(|i| format!("QQQ{i}")).collect();
    let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
    spots.extend(run(&transmission_events(1, &filler_refs, 100_000), &mut v));
    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must not spot yet -- CQ is still in the window, got {spots:?}"
    );

    // One more word evicts CQ (the oldest of the now-17) and is itself a
    // brand-new, standalone "K5ARH" with no trailing T.
    spots.extend(run(&transmission_events(1, &["K5ARH"], 300_000), &mut v));

    assert!(
        !spots
            .iter()
            .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
        "K5ARH must still not spot as Beacon -- the only match is the \
         original, already-suppressed CQ DX K5ARH T occurrence; the new \
         standalone K5ARH has no trailing T and must not inherit the old \
         match's identity, got {spots:?}"
    );
}
