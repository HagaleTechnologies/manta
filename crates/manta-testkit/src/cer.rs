//! Character error rate (Levenshtein distance / expected length) and word-boundary accuracy (`word_boundary_fraction`, MAN-173).

fn normalize(s: &str) -> Vec<char> {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
        .chars()
        .collect()
}

/// Character error rate: Levenshtein distance over expected length,
/// case/whitespace-normalized. Per ROADMAP.md's CER convention.
pub fn cer(expected: &str, decoded: &str) -> f64 {
    let e = normalize(expected);
    let d = normalize(decoded);
    if e.is_empty() {
        return if d.is_empty() { 0.0 } else { 1.0 };
    }
    // Two-row Levenshtein DP.
    let mut prev: Vec<usize> = (0..=d.len()).collect();
    let mut cur = vec![0usize; d.len() + 1];
    for i in 1..=e.len() {
        cur[0] = i;
        for j in 1..=d.len() {
            let sub = prev[j - 1] + usize::from(e[i - 1] != d[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[d.len()] as f64 / e.len() as f64
}

/// 1 - CER. Per ROADMAP.md's CER convention.
pub fn char_accuracy(expected: &str, decoded: &str) -> f64 {
    1.0 - cer(expected, decoded)
}

/// CER of `expected` against the frequency-joined copy of `tracks`
/// (MAN-116 D6, shared by MAN-120/FU-3): the texts of every track whose
/// frequency sits within `match_hz` of `freq_hz`, joined in ascending
/// track-id order with single spaces, empty texts skipped. Fading
/// fragments one station into several tracks; scoring against only one
/// of them (nearest, lowest id, ...) would score a fragmented station as
/// lost even when its copy is fine. With no track in the window the
/// joined copy is empty and the CER is 1. The caller applies its own
/// `.min(1.0)`/ceilings -- this returns the raw ratio.
pub fn joined_cer(expected: &str, tracks: &[(u64, f64, &str)], freq_hz: f64, match_hz: f64) -> f64 {
    let mut copy: Vec<(u64, &str)> = tracks
        .iter()
        .filter(|&(_, f, text)| (f - freq_hz).abs() <= match_hz && !text.is_empty())
        .map(|&(id, _, text)| (id, text))
        .collect();
    copy.sort_by_key(|&(id, _)| id);
    let joined = copy.into_iter().map(|(_, text)| text).collect::<Vec<_>>();
    cer(expected, &joined.join(" "))
}

/// What a cheapest character-level alignment did with one position in `a`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AlignOp {
    /// Aligned to an identical character in `b`.
    Match,
    /// Aligned to a different character in `b`.
    Sub,
    /// No corresponding character in `b` at all -- genuinely absent, not
    /// just misplaced.
    Del,
}

/// Full character-level Levenshtein alignment between `a` and `b`, with
/// traceback. Returns, for each index in `a`, which operation a cheapest
/// alignment applied there -- since a match requires literal equality, a
/// matched space in `a` can only align to a space in `b`. Distinguishing
/// `Sub` from `Del` (not just "not matched") matters for
/// `word_boundary_fraction`'s warm-up forgiveness: a substituted boundary
/// means the character is still *there*, just wrong (e.g. a split moved
/// it), while a deleted boundary means it's genuinely *absent* -- only the
/// latter is what SPEC §2.1's warm-up content loss actually looks like.
/// Ties among multiple equally-cheap explanations are broken
/// deterministically (prefer match, then substitution, then deletion, then
/// insertion); the choice never affects the total edit distance, only
/// which position is credited when several alignments are equally cheap.
fn char_alignment_ops(a: &[char], b: &[char]) -> Vec<AlignOp> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = dp[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]);
            dp[i][j] = sub.min(dp[i - 1][j] + 1).min(dp[i][j - 1] + 1);
        }
    }
    let mut ops = vec![AlignOp::Del; n];
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] && dp[i - 1][j - 1] == dp[i][j] {
            ops[i - 1] = AlignOp::Match;
            i -= 1;
            j -= 1;
        } else if i > 0 && j > 0 && dp[i - 1][j - 1] + 1 == dp[i][j] {
            ops[i - 1] = AlignOp::Sub;
            i -= 1;
            j -= 1;
        } else if i > 0 && dp[i - 1][j] + 1 == dp[i][j] {
            ops[i - 1] = AlignOp::Del;
            i -= 1;
        } else {
            j -= 1;
        }
    }
    ops
}

/// Word-boundary accuracy per the Task 10 brief, corrected per Codex review
/// (PR #161, seven rounds -- kept as a full account since this is exactly
/// the "same code region, new bug shape each round" pattern that should
/// prompt a redesign rather than another point patch; round 4 is that
/// redesign, rounds 5 and 7 correct its warm-up-forgiveness precondition
/// in opposite directions -- round 5 too loose, round 7 fixes round 5
/// overcorrecting to too strict; round 6 is a documented known limitation,
/// not a fix, see the test with "known_limitation" in its name):
///
/// Round 1 replaced a word-COUNT comparison with word-level edit distance,
/// since a decode that splits/merges every word (e.g. expected "CQ TEST
/// W5AU" decoded as "CQT EST W5AU") has matching word counts but zero
/// correct boundaries, and the count-based version credited it as 100%.
///
/// Round 2: word-level edit distance also penalizes a correctly-delimited
/// word that's merely misspelled (expected "CQ TEST" decoded "CQ TAST" has
/// the one boundary exactly right, but token inequality scores it as a
/// substitution) -- double-penalizing a character error CER already gates
/// separately. Tried comparing WORD-LENGTH sequences instead of the words
/// themselves.
///
/// Round 3: the length-sequence version double-counts a single real merge
/// or split (e.g. "CQ TEST W5AU W5AU" -> "CQ TESTW5AU W5AU") as a
/// delete+substitute pair. Tried extending the edit-distance DP with
/// merge/split as their own unit-cost transitions.
///
/// Round 4 (this version): the length-sequence approach, even with
/// merge/split fixed, still conflates a length-changing character error
/// INSIDE an otherwise correctly-delimited word (e.g. "CQ TEST W5AU" ->
/// "CQ TTEST W5AU", one inserted letter) with a real boundary error,
/// because a word's length changing looks identical in a length-only
/// comparison regardless of *why* it changed. No further patch to a
/// length/token proxy can fix this -- length and token content are both
/// lossy views of the same underlying character stream, so the actual
/// fix is to work on that stream directly: align `expected` and `decoded`
/// character-for-character (the same Levenshtein distance `cer` uses)
/// and check whether each SPACE character in `expected` survives that
/// alignment as a clean match. A match is only possible against an
/// identical character, so a matched space can only be aligned to a space
/// -- this one primitive correctly resolves all of rounds 1-4's examples
/// simultaneously (verified by direct test below), because it's actually
/// answering "is this specific boundary reproduced," not approximating it.
///
/// Warm-up forgiveness (SPEC §2.1's mandatory warm-up floor costs exactly
/// the first word in production) is narrower here than earlier rounds'
/// "drop the first word and realign" trick: that trick also credits a
/// SPLIT of the first word as if it were a clean drop (there's always some
/// cheaper alignment once you stop requiring the first word's content to
/// appear at all), which is a worse bug than the warm-up case it was
/// solving.
///
/// Round 4's own replacement -- forgive the first boundary whenever every
/// OTHER boundary already matches -- turned out to have the exact same
/// gap one level down: with 3+ words, a first-word SPLIT (e.g. "CQ TEST
/// W5AU" -> "CQT EST W5AU") can leave every boundary from the second one
/// onward matching cleanly, satisfying "everything else is right" without
/// any real content loss at all (round 5's regression). A substituted
/// boundary means the character is still THERE, just wrong; only a
/// DELETED boundary -- and, more precisely, the whole first word's worth
/// of characters being deletions, not just its trailing space -- is what
/// genuine warm-up content loss actually looks like (`AlignOp::Del`, not
/// merely "not `Match`"). Round 5 required that, but ALSO tied forgiveness
/// to "every other boundary already right," which over-corrected: the
/// original Task 10 semantics forgive the lost first word unconditionally
/// (add its credit, then count every other boundary independently), not
/// "only if nothing else went wrong either" (round 7, Codex review PR
/// #161: expected "CQ TEST W5AU W5AU" decoded "TEST W5AUW5AU" has a
/// genuinely dropped first word AND a separate real merge later -- the
/// right answer is 2/3 (1 forgiven + 1 of the remaining 2 correct), not
/// 1/3 as round 5's version produced). Each boundary is now scored
/// independently: boundary 0 is forgiven if genuinely dropped or credited
/// if it matches; every other boundary is credited only if it matches, on
/// its own, regardless of boundary 0's outcome.
///
/// MAN-173: rounds 1-7 all scored recall -- the share of `expected`'s
/// boundaries the decode reproduced -- so a decode that ADDED word breaks
/// kept 1.0 however many it added: V10's stage-2 HSMM decode, a space
/// between every character, would have scored a perfect 1.0, and VR7's
/// CER and boundary bars both passed its real decode with 17 calls split
/// in two. A decoded space that no expected space matched is a spurious
/// boundary and costs what a missed one costs: the score is
/// (credited - spurious) / expected boundaries, floored at 0 -- 1 minus a
/// boundary error rate, as char accuracy is 1 - CER. A moved boundary
/// ("CQ TEST" -> "CQT EST") is two errors, as it is on air: a word gap
/// read as a character gap and a character gap read as a word gap. With
/// no expected boundary, a decode scores 1 only if it adds none.
pub fn word_boundary_fraction(expected: &str, decoded: &str) -> f64 {
    let normalize = |s: &str| -> Vec<char> {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .collect()
    };
    let e = normalize(expected);
    let d = normalize(decoded);
    let boundaries: Vec<usize> = e
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == ' ')
        .map(|(i, _)| i)
        .collect();
    let decoded_boundaries = d.iter().filter(|&&c| c == ' ').count();
    if boundaries.is_empty() {
        return if decoded_boundaries == 0 { 1.0 } else { 0.0 };
    }
    let ops = char_alignment_ops(&e, &d);
    let boundary0_genuinely_dropped = ops[..=boundaries[0]].iter().all(|&op| op == AlignOp::Del);
    let matched = boundaries
        .iter()
        .filter(|&&p| ops[p] == AlignOp::Match)
        .count();
    let credited = matched + usize::from(boundary0_genuinely_dropped);
    // A `Match` pairs one expected space with one decoded space, so every
    // other decoded space is a word break `expected` does not have.
    let spurious = decoded_boundaries - matched;
    credited.saturating_sub(spurious) as f64 / boundaries.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_zero() {
        assert_eq!(cer("CQ CQ DE W1AW", "CQ CQ DE W1AW"), 0.0);
        assert_eq!(cer("CQ  CQ", "cq cq"), 0.0); // normalization
    }

    #[test]
    fn substitution_counts() {
        // 1 edit over 5 chars
        assert!((cer("PARIS", "PARIX") - 0.2).abs() < 1e-9);
    }

    #[test]
    fn insertion_and_deletion_count() {
        assert!((cer("PARIS", "PARIS5") - 0.2).abs() < 1e-9);
        assert!((cer("PARIS", "PAIS") - 0.2).abs() < 1e-9);
        assert!((char_accuracy("PARIS", "PARIS") - 1.0).abs() < 1e-9);
    }

    // joined_cer (MAN-120, the MAN-116 D6 frequency-join convention).

    #[test]
    fn joins_two_fragments_in_id_order() {
        // The vec lists the later fragment first; the join follows the
        // ids, so the two halves read in transmission order.
        let tracks = [
            (7u64, 14_000_060.0f64, "W1AW K"),
            (3u64, 14_000_000.0f64, "CQ CQ DE"),
        ];
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_030.0, 150.0),
            0.0
        );
        // In vec order instead, the halves would read backwards.
        assert!(cer("CQ CQ DE W1AW K", "W1AW K CQ CQ DE") > 0.25);
    }

    #[test]
    fn skips_empty_text_tracks() {
        // An empty-text track within the window contributes nothing --
        // not even a stray space in the join.
        let with_empty = [
            (1u64, 14_000_000.0f64, ""),
            (2u64, 14_000_010.0, "CQ CQ DE W1AW K"),
        ];
        let without: &[(u64, f64, &str)] = &[(2u64, 14_000_010.0, "CQ CQ DE W1AW K")];
        let expected = cer("CQ CQ DE W1AW K", "CQ CQ DE W1AW K");
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &with_empty, 14_000_005.0, 150.0),
            expected
        );
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", without, 14_000_005.0, 150.0),
            expected
        );
    }

    #[test]
    fn ignores_tracks_beyond_match_hz() {
        // A nearer-but-out-of-window track must not contribute.
        let tracks = [
            (1u64, 14_000_100.0f64, "COMPLETELY WRONG"),
            (2u64, 14_000_400.0, "CQ CQ DE W1AW K"),
        ];
        // 14_000_400 is 300 Hz from 14_000_100: outside a 150 Hz window
        // around the probe, inside a 350 Hz one.
        assert!(joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_100.0, 150.0) > 0.8);
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_400.0, 150.0),
            0.0
        );
    }

    #[test]
    fn no_tracks_within_window_is_cer_one() {
        let tracks = [(1u64, 14_000_000.0f64, "CQ CQ DE W1AW K")];
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_001_000.0, 150.0),
            1.0
        );
        assert_eq!(joined_cer("CQ CQ DE W1AW K", &[], 14_001_000.0, 150.0), 1.0);
    }

    // word_boundary_fraction (MAN-173; moved from golden_vr.rs by MAN-549).

    #[test]
    fn word_boundary_fraction_ignores_misspellings_at_correctly_placed_boundaries() {
        // Round 2's case: one boundary, exactly right, character content wrong.
        let wb = word_boundary_fraction("CQ TEST", "CQ TAST");
        assert!((wb - 1.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_flags_a_split() {
        // "CQT EST" splits "CQ TEST" at the wrong point -- 0 of 1 boundaries right.
        let wb = word_boundary_fraction("CQ TEST", "CQT EST");
        assert!((wb - 0.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_counts_a_single_merge_as_one_error_not_two() {
        // Round 3's case: one real merge among 3 boundaries. 2 of 3 boundaries
        // (the one before and the one after the merge point) are still correct.
        let wb = word_boundary_fraction("CQ TEST W5AU W5AU", "CQ TESTW5AU W5AU");
        assert!((wb - 2.0 / 3.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_penalizes_a_pure_extra_split() {
        // MAN-173: the mirror of the merge case. Splitting "TESTW5AU" adds a
        // decoded space no expected space matches -- a spurious boundary --
        // and it costs what a missed one would: 1 - 1/2.
        let wb = word_boundary_fraction("CQ TESTW5AU W5AU", "CQ TEST W5AU W5AU");
        assert!((wb - 0.5).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_costs_an_extra_split_the_same_as_a_merge() {
        // MAN-173: over- and under-segmentation are symmetric. One merge and
        // one extra split each cost one boundary of three.
        let merged = word_boundary_fraction("CQ TEST W5AU W5AU", "CQ TESTW5AU W5AU");
        let split = word_boundary_fraction("CQ TEST W5AU W5AU", "CQ TEST W5 AU W5AU");
        assert!((merged - 2.0 / 3.0).abs() < 1e-9, "merge: got {merged}");
        assert!((split - 2.0 / 3.0).abs() < 1e-9, "split: got {split}");
    }

    #[test]
    fn word_boundary_fraction_scores_a_split_between_every_character_as_zero() {
        // MAN-173: V10's stage-2 HSMM failure, a space between every
        // character (docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md
        // section 3), reproduces all 4 expected boundaries and adds 17 more.
        let wb = word_boundary_fraction(
            "CQ TEST JA1ZZZ JA1ZZZ TEST",
            "C Q T E S T J A 1 Z Z Z J A 1 Z Z Z T E S T",
        );
        assert!((wb - 0.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_penalizes_a_split_of_a_single_word() {
        // No expected boundary to divide by: a decode that adds one scores 0,
        // one that adds none scores 1, as `manta_testkit::cer` treats an
        // empty expected text.
        let wb = word_boundary_fraction("TEST", "TE ST");
        assert!((wb - 0.0).abs() < 1e-9, "got {wb}");
        let wb = word_boundary_fraction("TEST", "TEST");
        assert!((wb - 1.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_counts_an_extra_split_after_a_dropped_first_word() {
        // Warm-up forgiveness and the spurious-boundary penalty are
        // independent, as round 7 made forgiveness independent of misses:
        // 1 forgiven + 2 matched - 1 spurious ("W5 AU") = 2 of 3.
        let wb = word_boundary_fraction("CQ TEST W5AU W5AU", "TEST W5 AU W5AU");
        assert!((wb - 2.0 / 3.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_ignores_an_internal_length_changing_character_error() {
        // Round 4's case: an inserted letter INSIDE a word ("TEST" -> "TTEST")
        // changes that word's length but not either surrounding boundary's
        // placement -- both boundaries still land correctly. CER (not this
        // metric) is what should catch the extra letter.
        let wb = word_boundary_fraction("CQ TEST W5AU", "CQ TTEST W5AU");
        assert!((wb - 1.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_forgives_a_genuinely_dropped_first_word() {
        // SPEC §2.1's warm-up floor: the decoder can lose the whole opening
        // word in real production use. Everything else lands correctly, so the
        // one missing leading boundary is forgiven.
        let wb = word_boundary_fraction("CQ TEST W5AU", "TEST W5AU");
        assert!((wb - 1.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_does_not_forgive_a_split_first_word() {
        // The narrow forgiveness rule must not be fooled by a first-word SPLIT
        // (no content actually lost, just misplaced) into crediting it as a
        // clean drop -- with only 1 boundary total there's nothing else to
        // confirm "everything else is right" against, so no forgiveness applies.
        let wb = word_boundary_fraction("CQ TEST", "CQT EST");
        assert!((wb - 0.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_does_not_forgive_a_split_first_word_with_more_words_after() {
        // Round 5's regression: with 3+ words, a first-word split can leave
        // EVERY later boundary matching cleanly (the disruption is local), so
        // "every other boundary already right" alone isn't enough to prove
        // genuine content loss -- must also require the drop to actually be a
        // deletion, not a misplaced substitution. MAN-173: the moved
        // boundary is two errors, missed after "CQ" and spurious after "CQT",
        // so (1 - 1)/2 = 0; a wrongly forgiven first boundary would read 0.5.
        let wb = word_boundary_fraction("CQ TEST W5AU", "CQT EST W5AU");
        assert!((wb - 0.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_forgives_the_drop_independently_of_other_errors() {
        // Round 7 (Codex review, PR #161): dropping the first word AND
        // breaking a later boundary (a merge, here) are two INDEPENDENT
        // errors -- the genuine drop is still forgiven on its own terms, and
        // the separate merge is counted on its own terms. 1 forgiven + 1 of
        // the remaining 2 correct (the middle boundary, "TEST"/"W5AUW5AU" is
        // the one that's wrong) = 2/3. Round 5's earlier version wrongly
        // required "every OTHER boundary already right" as a precondition for
        // forgiving the drop at all, giving 1/3 here -- overcorrecting for
        // round 5's own bug in the opposite direction.
        let wb = word_boundary_fraction("CQ TEST W5AU W5AU", "TEST W5AUW5AU");
        assert!((wb - 2.0 / 3.0).abs() < 1e-9, "got {wb}");
    }

    #[test]
    fn word_boundary_fraction_known_limitation_repeated_structure_can_launder_a_boundary() {
        // Round 6 (Codex review, PR #161): with specific repeated-character
        // structure near a split, the globally cheapest character alignment
        // can pair an expected boundary with a DIFFERENT decoded space than
        // the one a human would consider "the same boundary" -- e.g. the space
        // introduced by garbling "TEST" into "E ST" gets credited as if it
        // were the boundary after "CQ", because that's cheaper overall than
        // the alignment that would correctly flag it as substituted/deleted.
        // Character-level Levenshtein has no notion of "boundary identity"; it
        // only knows individual characters matched or didn't, in whichever
        // globally-cheapest way. Closing this fully needs a WORD-level
        // alignment with a content-similarity-aware substitution cost (neither
        // exact-equality, round 2's flaw, nor length-only, rounds 3-4's flaw)
        // -- a materially larger redesign than a character-alignment patch,
        // and out of scope for this review round. Documented here, not
        // silently accepted: this exact input is a known false positive.
        // MAN-173 narrowed it: the decode has 5 spaces to the 4 the alignment
        // matched, so one spurious boundary now costs 1/4 (1.0 -> 0.75). The
        // laundered pairing still hides the missed boundary after "CQ" and a
        // second spurious one; the correct score is 1 - (1 + 2)/4 = 0.25.
        let wb = word_boundary_fraction("CQ TEST W5AU W5AU TEST", "CQT E ST W5AU W5AU TEST");
        assert!(
            (wb - 0.75).abs() < 1e-9,
            "documenting the known false positive (got {wb}, expected exactly the buggy 0.75; \
             correct is 0.25) -- \
             if this assertion ever fails, the bug is FIXED, not broken; update this test to assert \
             the correct value instead of reverting whatever fixed it"
        );
    }
}
