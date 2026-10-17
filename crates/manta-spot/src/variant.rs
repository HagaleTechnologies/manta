//! MAN-100: is one decoded candidate a fading-corrupted form of another?
//!
//! Two arms, both measured against a 50-signal CCIR-poor pileup fixture
//! (see `docs/DECISIONS/2026-09-07-man100-variant-arbitration.md`): a
//! *containment* arm for truncation/merge, and a *near-miss* arm for a
//! shared prefix with a divergent tail -- 3 of 5 measured bogus calls in
//! that fixture were near-misses, not substrings, so a literal
//! sub/superstring check alone catches only 2 of 5.

/// Number of leading characters `a` and `b` share.
pub fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Levenshtein (edit) distance between `a` and `b`. Callsigns are capped
/// at 11 characters by `grammar::is_plausible` (a 7-character base plus
/// an optional `/` and up to a 3-character portable designator -- `/P`,
/// `/QRP`, `/MM`, `/AM`, `/M`, or `/<digit>`), so the plain two-row DP is
/// already trivially cheap -- no bounded/early-exit variant is warranted.
/// (`manta-testkit::cer` has a sibling implementation, but it's a
/// test-only crate that `manta-spot` must not depend on.)
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// How one candidate relates to another, when they're plausibly the same
/// station's callsign corrupted by fading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// One string is a contiguous substring of the other (truncation, or a
    /// framing word merged onto the call).
    Containment,
    /// Shared prefix >= `MIN_SHARED_PREFIX` chars, edit distance <=
    /// `MAX_EDIT_DISTANCE`, but neither contains the other (a divergent
    /// tail from fading corrupting characters after the callsign's more
    /// reliable leading run).
    NearMiss,
}

/// Shortest shared prefix that still means "same station, corrupted".
const MIN_SHARED_PREFIX: usize = 3;
/// Widest edit distance still consistent with one fading-corrupted decode
/// of the same underlying transmission.
const MAX_EDIT_DISTANCE: usize = 2;

/// Whether `a` and `b` are plausibly two corrupted decodes of the same
/// underlying callsign. Symmetric, and `None` for a call compared with
/// itself (identical strings are the same candidate, not two variants of
/// one another).
pub fn relation(a: &str, b: &str) -> Option<Relation> {
    if a == b {
        return None;
    }
    if a.contains(b) || b.contains(a) {
        return Some(Relation::Containment);
    }
    if common_prefix_len(a, b) >= MIN_SHARED_PREFIX && levenshtein(a, b) <= MAX_EDIT_DISTANCE {
        return Some(Relation::NearMiss);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_measured_v8w_failure_pair_is_related() {
        for (a, b) in [
            ("K6F", "K6FXJ"),
            ("W6JQ", "W6JQA"), // containment
            ("AB2TTLK", "AB2KLK"),
            ("W4KTNL", "W4KCL"),
            ("W6DW", "W6DPG"), // near-miss
        ] {
            assert!(relation(a, b).is_some(), "{a} vs {b} must be confusable");
            assert_eq!(relation(a, b), relation(b, a), "the relation is symmetric");
        }
    }

    #[test]
    fn containment_pairs_are_classified_as_containment() {
        assert_eq!(relation("K6F", "K6FXJ"), Some(Relation::Containment));
        assert_eq!(relation("W6JQ", "W6JQA"), Some(Relation::Containment));
    }

    #[test]
    fn near_miss_pairs_are_classified_as_near_miss() {
        assert_eq!(relation("AB2TTLK", "AB2KLK"), Some(Relation::NearMiss));
        assert_eq!(relation("W4KTNL", "W4KCL"), Some(Relation::NearMiss));
        assert_eq!(relation("W6DW", "W6DPG"), Some(Relation::NearMiss));
    }

    #[test]
    fn unrelated_calls_are_not_confusable() {
        for (a, b) in [("K5ARH", "W1AW"), ("VE3ABC", "JA1ABC"), ("K6FXJ", "N7CTR")] {
            assert_eq!(relation(a, b), None, "{a} vs {b} must not be confusable");
        }
    }

    #[test]
    fn a_call_is_never_a_variant_of_itself() {
        assert_eq!(relation("K5ARH", "K5ARH"), None);
    }

    #[test]
    fn distance_and_prefix_thresholds_hold_the_line() {
        // Only 2 shared leading chars -- not enough, even at distance 2.
        assert_eq!(relation("K6ABC", "K6XYZ"), None);
        // 3 edits apart is not a near-miss, however long the shared prefix.
        assert_eq!(relation("K5ARHXY", "K5ARZZZ"), None);
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("K5ARH", "K5ARH"), 0);
        assert_eq!(levenshtein("K5ARH", "K5AR"), 1);
        assert_eq!(levenshtein("AB2TTLK", "AB2KLK"), 2);
        assert_eq!(levenshtein("", "ABC"), 3);
    }

    #[test]
    fn common_prefix_len_counts_leading_shared_chars() {
        assert_eq!(common_prefix_len("K5ARH", "K5ARZ"), 4);
        assert_eq!(common_prefix_len("ABC", "XYZ"), 0);
        assert_eq!(common_prefix_len("ABC", "ABC"), 3);
    }
}
