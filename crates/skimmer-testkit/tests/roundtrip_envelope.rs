//! text -> keyed envelope (375 Hz) -> TrackDecoder -> text, CER = 0.
//! Isolates the decode chain from the DSP front end.

use proptest::prelude::*;
use skimmer_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use skimmer_testkit::cer::cer;
use skimmer_testkit::keyer::{key_text, KeyerSpec};

/// Restricts the generated text's first character to avoid an all-dah
/// opening (pinned decision 20, `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`):
/// `ClusterPair`'s unimodal-init branch (`crates/skimmer-decode/src/timing.rs`)
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
        .filter_map(skimmer_decode::tree::pattern_for)
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn clean_envelope_roundtrip(text in text_strategy(), wpm in 10.0f32..=40.0) {
        let (env, keyed) = key_text(&text, &KeyerSpec::new(wpm), 375.0).unwrap();
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
        let decoded = events_to_text(&events);
        prop_assert_eq!(cer(&keyed, &decoded), 0.0, "keyed {:?} decoded {:?}", keyed, decoded);
    }
}
