//! text -> keyed envelope (375 Hz) -> TrackDecoder -> text, CER = 0.
//! Isolates the decode chain from the DSP front end.

use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_testkit::cer::cer;
use manta_testkit::keyer::{key_text, Jitter, KeyerSpec};
use proptest::prelude::*;

/// Restricts the generated text's first character to avoid an all-dah
/// opening (pinned decision 20, `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`):
/// `ClusterPair`'s unimodal-init branch (`crates/manta-decode/src/timing.rs`)
/// always assumes the first 5-mark cluster is dits and can't recover if it
/// turns out to be a homogeneous run of dahs instead (e.g. M, O, or
/// repeated T). This is NOT "must contain both dit and dah elements" —
/// several excluded letters (B, J, U) contain both element types and decode
/// fine; the real constraint is narrower than that.
const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn word_strategy(first: bool) -> impl Strategy<Value = String> {
    let head = if first {
        proptest::sample::select(MIXED_FIRST.chars().collect::<Vec<_>>())
    } else {
        proptest::sample::select(REST.chars().collect::<Vec<_>>())
    };
    (
        head,
        proptest::collection::vec(
            proptest::sample::select(REST.chars().collect::<Vec<_>>()),
            1..6,
        ),
    )
        .prop_map(|(h, tail)| {
            let mut w = h.to_string();
            w.extend(tail);
            w
        })
}

/// Total dit/dah elements (marks) across the whole text, ignoring spaces.
/// SPEC §4.1: the online 2-means speed tracker only becomes `ready()` after
/// its 5th mark; below that quorum no character can ever be classified
/// (`TrackDecoder::finish` requires `tracker.ready()` too, so the buffered
/// runs are silently dropped, not just delayed). A minimal-length text like
/// "AA" (2+2 = 4 marks) can never decode by design — that's not a decode
/// bug, it's the same "no ratio reference yet" limitation the MIXED_FIRST
/// precondition above already exists to avoid, just not fully closed by a
/// first-character-only constraint. Filter it out here rather than in
/// decode-chain code.
fn total_marks(text: &str) -> usize {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter_map(manta_decode::tree::pattern_for)
        .map(str::len)
        .sum()
}

fn text_strategy() -> impl Strategy<Value = String> {
    (
        word_strategy(true),
        proptest::collection::vec(word_strategy(false), 0..3),
    )
        .prop_map(|(first, rest)| {
            let mut words = vec![first];
            words.extend(rest);
            words.join(" ")
        })
        .prop_filter(
            "at least 5 marks total (SPEC §4.1 tracker init quorum)",
            |t| total_marks(t) >= 5,
        )
}

/// One text -> envelope -> decoder round trip. Returns `(keyed, decoded)`.
fn roundtrip(text: &str, wpm: f32) -> (String, String) {
    let (env, keyed) = key_text(text, &KeyerSpec::new(wpm), 375.0).unwrap();
    let mut dec = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for (i, &a) in env.iter().enumerate() {
        events.extend(dec.push_envelope(a, i as u64 * 256));
    }
    // Trailing silence: enough for the 7-dit flush AND to guarantee the
    // demod's 375-hop init window fills even for the shortest texts
    // (a 2-char word at 40 WPM is only ~170 hops of envelope).
    let tail = (8.0 * 1200.0 / wpm * 0.375) as usize + 450;
    for i in 0..tail {
        events.extend(dec.push_envelope(0.0, (env.len() + i) as u64 * 256));
    }
    events.extend(dec.finish());
    (keyed, events_to_text(&events))
}

/// `roundtrip` with an explicit keyer and a fixed tail of 900 silent hops
/// (MAN-315: past every flush, and the demod's 375-hop init window).
fn roundtrip_spec(text: &str, spec: &KeyerSpec) -> (String, String) {
    let (env, keyed) = key_text(text, spec, 375.0).unwrap();
    let mut dec = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for (i, &a) in env.iter().chain([0.0f32; 900].iter()).enumerate() {
        events.extend(dec.push_envelope(a, i as u64 * 256));
    }
    events.extend(dec.finish());
    (keyed, events_to_text(&events))
}

fn spaced(wpm: f32, char_gap_units: f32, word_gap_units: f32) -> KeyerSpec {
    KeyerSpec {
        char_gap_units,
        word_gap_units,
        ..KeyerSpec::new(wpm)
    }
}

/// MAN-315 scenario 2: letter gaps of 4.5 dits and word gaps of 9. Gaps
/// measured in mark dits read long by the rise time, so every letter gap
/// crossed the 5.0 word boundary ("C Q T E S T W 1 A W", measured).
#[test]
fn long_letter_gaps_give_exactly_three_words() {
    let (_, decoded) = roundtrip_spec("CQ TEST W1AW", &spaced(25.0, 4.5, 9.0));
    assert_eq!(decoded, "CQ TEST W1AW");
}

#[test]
fn long_letter_gaps_hold_across_speeds_and_jitter() {
    let text = "CQ TEST W1AW CQ TEST W1AW";
    for wpm in [15.0f32, 25.0, 35.0] {
        let (_, decoded) = roundtrip_spec(text, &spaced(wpm, 4.5, 9.0));
        assert_eq!(
            decoded.split_whitespace().count(),
            6,
            "at {wpm} WPM decoded {decoded:?}"
        );
    }
    let spec = KeyerSpec {
        jitter: Some(Jitter {
            sigma: 0.05,
            seed: 1,
        }),
        ..spaced(25.0, 4.5, 9.0)
    };
    let (_, decoded) = roundtrip_spec(&[text, "CQ TEST W1AW"].join(" "), &spec);
    assert_eq!(decoded.split_whitespace().count(), 9, "decoded {decoded:?}");
}

/// MAN-315 D4: the learned word boundary must not merge tight spacing
/// (VR7-like 2.5/5 at 35 WPM with jitter). The first word gap, before any
/// class is learned, is warm-up.
#[test]
fn tight_spacing_still_splits_words() {
    let spec = KeyerSpec {
        jitter: Some(Jitter {
            sigma: 0.08,
            seed: 4,
        }),
        ..spaced(35.0, 2.5, 5.0)
    };
    let (_, decoded) = roundtrip_spec("CQ TEST JA1ZZZ JA1ZZZ TEST", &spec);
    assert!(
        decoded.ends_with("JA1ZZZ JA1ZZZ TEST"),
        "decoded {decoded:?}"
    );
}

/// MAN-315 scenario 1 (clean lock-in): heavy dashes decode exactly.
#[test]
fn heavy_dashes_decode_exactly() {
    for weight in [4.0f32, 4.5, 5.0] {
        let spec = KeyerSpec {
            weight,
            ..KeyerSpec::new(25.0)
        };
        let (keyed, decoded) = roundtrip_spec("CQ CQ DE K1ABC K1ABC K", &spec);
        assert_eq!(decoded, keyed, "weight {weight}");
    }
}

/// MAN-103 regression, pinned deterministically. `clean_envelope_roundtrip`
/// below is an unseeded 64-case proptest, so the case it shrank to during
/// MAN-103's validation (`"GQH AA"` at 39.95 WPM decoding as `"GQH A A"`)
/// only reproduced about one run in five. The mechanism: at the top of the
/// supported speed range a plain *element* gap measures a little over one
/// dit, and the Farnsworth long-gap floor (`FARNS_LONG_U`,
/// `crates/manta-decode/src/timing.rs`) admitted it into the long-gap
/// clusters, dragging the word threshold down until real inter-character
/// gaps classified as inter-word. Any speed near 40 WPM with adjacent
/// repeated characters exercises it.
#[test]
fn high_wpm_element_gap_does_not_force_a_word_break() {
    for &wpm in &[38.0f32, 39.952156, 40.0] {
        let (keyed, decoded) = roundtrip("GQH AA", wpm);
        assert_eq!(
            cer(&keyed, &decoded),
            0.0,
            "at {wpm} WPM: keyed {keyed:?} decoded {decoded:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn clean_envelope_roundtrip(text in text_strategy(), wpm in 10.0f32..=40.0) {
        let (keyed, decoded) = roundtrip(&text, wpm);
        prop_assert_eq!(cer(&keyed, &decoded), 0.0, "keyed {:?} decoded {:?}", keyed, decoded);
    }
}
