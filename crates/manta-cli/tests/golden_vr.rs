//! SPEC v2 §8.2 "real-conditions" golden gates VR1-VR8. Conditions the
//! original V1-V10 vectors never exercised (contest speed, deep keying, hard
//! transmitter edges, co-channel occupancy, non-standard weighting, tight
//! Farnsworth-free spacing) -- the gap that let the real-signal timing
//! defect motivating this redesign go undetected.
//!
//! All nine tests here run `manta decode --json --engine hsmm` (SPEC v2's
//! stage-2 engine). Task 12 (2026-09-09) measured the real `HsmmDecoder`
//! against every one and un-ignored the three that clear their bars
//! (vr6a, vr6b, vr7); the other six (vr1-vr5, vr8) stay
//! `#[ignore = "stage-2 gate, un-ignored by Task 12"]` -- see
//! docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md for the
//! measured CER/WPM/word-boundary numbers and failure-kind analysis. The
//! overall stage-2 gate (SPEC v2 §8.4) is FAIL.

use std::collections::HashSet;
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json", "--engine", "hsmm"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

/// Group `report["events"]` by `track_id`, returning each track's decoded
/// text and its last-reported TrackMeta freq_hz. Same shape as
/// golden_v7_v9_v10.rs's/golden_v8_v8w.rs's helper of the same name.
fn per_track(report: &serde_json::Value) -> std::collections::BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
    let mut freqs: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = ev["track_id"].as_u64().unwrap();
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"]["Char"].as_str() {
                    texts.entry(tid).or_default().push_str(c);
                }
            }
            "WordBoundary" => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            "TrackMeta" => {
                freqs.insert(tid, ev["freq_hz"].as_f64().unwrap());
            }
            _ => {}
        }
    }
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

/// The decoded track whose last-reported freq_hz is closest to
/// `expected_freq`.
fn track_nearest_freq(
    tracks: &std::collections::BTreeMap<u64, (String, Option<f64>)>,
    expected_freq: f64,
) -> &str {
    tracks
        .values()
        .min_by(|(_, fa), (_, fb)| {
            let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
            let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
            da.partial_cmp(&db).unwrap()
        })
        .map(|(text, _)| text.as_str())
        .unwrap()
}

/// `(callsign, track_id)` pairs from `report["spots"]` -- the real
/// `manta-spot::Validator`'s output. Same shape as golden_v8_v8w.rs.
fn spotted_calls(report: &serde_json::Value) -> Vec<(String, u64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["callsign"].as_str().unwrap().to_string(),
                s["track_id"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Extract the callsign from this project's SPEC v2 §8.2 payload template
/// "CQ TEST <CALL> <CALL> TEST" (word index 2).
fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(2)
        .expect("keyed text must follow the 'CQ TEST <CALL> <CALL> TEST' template")
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
/// character-for-character (Levenshtein, same as `manta_testkit::cer`)
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
fn word_boundary_fraction(expected: &str, decoded: &str) -> f64 {
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
    if boundaries.is_empty() {
        return 1.0;
    }
    let ops = char_alignment_ops(&e, &d);
    let boundary0_genuinely_dropped = ops[..=boundaries[0]].iter().all(|&op| op == AlignOp::Del);
    let correct = boundaries
        .iter()
        .enumerate()
        .filter(|&(idx, &p)| ops[p] == AlignOp::Match || (idx == 0 && boundary0_genuinely_dropped))
        .count();
    correct as f64 / boundaries.len() as f64
}

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
fn word_boundary_fraction_does_not_penalize_a_pure_extra_split() {
    // NOT a mirror of the merge case, deliberately: splitting a word adds a
    // space that a Levenshtein alignment can absorb as a pure insertion
    // without disturbing either of expected's own boundaries (unlike a
    // merge, which genuinely deletes an expected space, i.e. loses a real
    // boundary). This metric measures recall of EXPECTED boundaries, not
    // precision against decoded's own extra ones -- a decoded-side-only
    // extra split, elsewhere confirmed correct, scores 1.0, not 2/3.
    let wb = word_boundary_fraction("CQ TESTW5AU W5AU", "CQ TEST W5AU W5AU");
    assert!((wb - 1.0).abs() < 1e-9, "got {wb}");
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
    // deletion, not a misplaced substitution.
    let wb = word_boundary_fraction("CQ TEST W5AU", "CQT EST W5AU");
    assert!((wb - 0.5).abs() < 1e-9, "got {wb}");
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
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr1_contest_40_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr1();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "VR1 char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 40.0).abs() < 2.0, "wpm {wpm}");
    let wb = word_boundary_fraction(&manifest.keyed_texts[0], decoded);
    assert!(wb >= 0.95, "VR1 word boundaries must be >= 95%, got {wb}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr2_contest_45_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr2();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "VR2 char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 45.0).abs() < 3.0, "wpm {wpm}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr3_deep_keying_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr3();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.02,
        "VR3 char accuracy must be >= 98% (CER <= 0.02), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // "Measured dit within +/-10% of 40 ms" (VR3 is 30 WPM: nominal dit =
    // 1200/30 = 40 ms): derived from the reported WPM (dit_ms = 1200/wpm),
    // since `decode --json` has no direct per-element timing field.
    let wpm = report["wpm"].as_f64().unwrap();
    let dit_ms = 1200.0 / wpm;
    assert!(
        (dit_ms - 40.0).abs() <= 4.0,
        "measured dit {dit_ms:.2} ms (from wpm {wpm}) outside +/-10% of 40 ms"
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr4_hard_edges_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr4();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "VR4 char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr5_co_channel_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr5();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);

    // Signal A (30 WPM, +25 dB, W1AA) is `manifest.keyed_texts[0]` /
    // `manifest.expected_freqs_hz[0]` -- SPEC v2 §8.2 only gates signal A.
    let decoded_a = track_nearest_freq(&tracks, manifest.expected_freqs_hz[0]);
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_a);
    assert!(
        cer <= 0.10,
        "VR5 signal A char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded_a
    );

    // 0 bogus callsigns: every spotted call must be one of the two known
    // co-channel signals' callsigns (W1AA, W2BB).
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    let spots = spotted_calls(&report);
    let bogus: Vec<&str> = spots
        .iter()
        .map(|(c, _)| c.as_str())
        .filter(|c| !known_calls.contains(c))
        .collect();
    assert!(
        bogus.is_empty(),
        "VR5 must spot 0 bogus callsigns, got {bogus:?}"
    );
}

/// Shared VR6 assertion: char >= 95% at the given dah:dit weight.
fn assert_vr6_passes(spec: &manta_testkit::vectors::VectorSpec) {
    let (report, manifest) = decode_report(spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "{} char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        spec.name,
        manifest.keyed_texts[0],
        decoded
    );
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr6a_weighting_2_6_passes_end_to_end() {
    assert_vr6_passes(&manta_testkit::vectors::vr6a());
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr6b_weighting_3_4_passes_end_to_end() {
    assert_vr6_passes(&manta_testkit::vectors::vr6b());
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr7_tight_spacing_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr7();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "VR7 char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wb = word_boundary_fraction(&manifest.keyed_texts[0], decoded);
    assert!(wb >= 0.90, "VR7 word boundaries must be >= 90%, got {wb}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr8_qsb_fast_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr8();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.15,
        "VR8 char accuracy must be >= 85% (CER <= 0.15), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
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
    let wb = word_boundary_fraction("CQ TEST W5AU W5AU TEST", "CQT E ST W5AU W5AU TEST");
    assert!(
        (wb - 1.0).abs() < 1e-9,
        "documenting the known false positive (got {wb}, expected exactly the buggy 1.0) -- \
         if this assertion ever fails, the bug is FIXED, not broken; update this test to assert \
         the correct value instead of reverting whatever fixed it"
    );
}
