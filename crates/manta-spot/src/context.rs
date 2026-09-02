//! CQ/DE/beacon context parse. ARCHITECTURE §6.1.
//!
//! Deliberately lightweight: matches the exact pattern families
//! ARCHITECTURE §6.1 lists (`CQ <call>`, `CQ TEST <call>`, `DE <call>`,
//! `<call> UP`, `V V V <call>`, `<call> T`). Filler words between the
//! keyword and the call (e.g. "CQ DX CQ DX DE ...", "CQ CONTEST ...") are a
//! known gap, not handled by this first pass -- same "tracked, not
//! blocking" treatment this project gives other classical-parsing
//! limitations (see the known decode bugs tracked as GitHub issues).

use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// The context a decoded callsign was found in. Carried on `Spot` as the
/// RBN spot-type flag (ARCHITECTURE §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SpotType {
    Cq,
    De,
    Beacon,
    Unknown,
}

static BEACON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bV\s+V\s+V\s+([A-Z0-9/]{3,15})\b").unwrap());
static DE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bDE\s+([A-Z0-9/]{3,15})\b").unwrap());
static CQ_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bCQ\b").unwrap());
static CQ_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bCQ(?:\s+TEST)?\s+([A-Z0-9/]{3,15})\b").unwrap());
static UP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Z0-9/]{3,15})\s+UP\b").unwrap());
/// NCDXF/IARU-style beacon ID: a callsign followed by four unmodulated
/// power-step dashes (100W/10W/1W/100mW), no spoken "V V V" preamble
/// (MAN-37). The decoder's on/off-keying detector can't resolve the
/// individual power steps out of what is, to it, one unbroken carrier --
/// there's no inter-element keying gap to split the four dashes on -- so
/// this decodes as the callsign followed by a single trailing "T" (one
/// dash), not "TTTT". A lone "T" is also the single most common
/// garbled/noise decode in CW, so this pattern is a known, accepted
/// source of false-positive `Beacon` tags (see MAN-37 decision notes) --
/// bounded blast radius since `Beacon` only lifts the repetition-gate
/// requirement (ARCHITECTURE §6 step 4), it doesn't bypass grammar/cty.
/// The trailing `T` must be a complete decoded word (end-of-text or
/// followed by whitespace), not just a word-boundary transition -- a bare
/// `\b` also matched a portable-designator suffix glued onto the same
/// word, e.g. "K5ARH T/QRP" (Codex review on PR #65).
static POWER_STEP_BEACON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Z0-9/]{3,15})\s+T(?:\s|$)").unwrap());
/// An unresolved `CQ`/`DE` token immediately (at most one word) before a
/// given position. A match here is only treated as genuinely unresolved
/// framing -- rather than the same `CQ`/`DE` a named pattern already
/// explained -- when it isn't fully covered by that named match's own
/// range (see `parse_power_step_beacons`): checking whether ANY named
/// pattern matched ANYWHERE in the text isn't enough, since an unrelated
/// resolved match elsewhere in the window must not excuse a DIFFERENT,
/// genuinely unresolved `CQ`/`DE` sitting right next to this candidate
/// (Codex review on PR #65, round 4). Scoped to the position right before
/// the candidate, not the whole window, so a stale, unrelated `CQ`/`DE`
/// several words earlier can't block a genuinely new, later beacon
/// occurrence either (round 3).
///
/// Allows 1-3 words between the keyword and this position, not just one:
/// a real CQ preamble commonly repeats the callsign ("CQ DX K5ARH
/// K5ARH"), so the immediately-preceding filler content can itself be
/// more than a single token (round 5) -- 3 covers `CQ`/`DE` + one
/// modifier word (`TEST`/`DX`/etc.) + the callsign said twice, the
/// longest realistic unresolved-framing shape seen across every round of
/// review so far. A stale match further back than that still can't reach
/// here at all (round 3's fix), so widening this bound doesn't reopen
/// that gap.
static CQ_DE_IMMEDIATE_FILLER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b((?:CQ|DE)(?:\s+\S+){1,3})\s*$").unwrap());

/// The `BEACON_RE`/`DE_RE`/`CQ_CALL_RE`/`UP_RE` family: the first (in that
/// priority order) named-keyword pattern that matches anywhere in `text`.
/// Returns the callsign candidate (uppercased), spot type, and match byte
/// range -- see `parse`'s own docs for what the range is for.
fn parse_named_pattern(text: &str) -> Option<(String, SpotType, Range<usize>)> {
    if let Some(caps) = BEACON_RE.captures(text) {
        let m = caps.get(0).unwrap();
        return Some((caps[1].to_uppercase(), SpotType::Beacon, m.range()));
    }
    if let Some(caps) = DE_RE.captures(text) {
        let de_match = caps.get(0).unwrap();
        if let Some(cq_match) = CQ_TOKEN_RE.find(text) {
            let start = de_match.start().min(cq_match.start());
            let end = de_match.end().max(cq_match.end());
            return Some((caps[1].to_uppercase(), SpotType::Cq, start..end));
        }
        return Some((caps[1].to_uppercase(), SpotType::De, de_match.range()));
    }
    if let Some(caps) = CQ_CALL_RE.captures(text) {
        let m = caps.get(0).unwrap();
        return Some((caps[1].to_uppercase(), SpotType::Cq, m.range()));
    }
    if let Some(caps) = UP_RE.captures(text) {
        let m = caps.get(0).unwrap();
        return Some((caps[1].to_uppercase(), SpotType::De, m.range()));
    }
    None
}

/// Every power-step fallback match in `text` (see `POWER_STEP_BEACON_RE`),
/// each its own independent candidate -- collapsing to a single "best"
/// match (whether first, last, or otherwise) proved unsound: two distinct,
/// never-yet-attempted power-step IDs can coexist in the window (e.g. a
/// track held back by the `has_meta` gate, MAN-28 round 8, so NEITHER got
/// a chance to be evaluated before both arrived), and dropping either one
/// permanently loses it (Codex review on PR #65, round 4). `Validator`'s
/// own per-word `attempted` state -- not this function -- is what stops a
/// genuinely already-resolved word from being reprocessed.
///
/// `named_range` is `parse_named_pattern`'s own match range, if any. A
/// candidate is dropped only when an unresolved `CQ`/`DE` token
/// immediately precedes it (see `CQ_DE_IMMEDIATE_FILLER_RE`) AND that
/// token isn't already fully explained by `named_range` -- checking
/// merely whether some named pattern matched ANYWHERE in the text isn't
/// enough, since an unrelated resolved match elsewhere must not excuse a
/// different, genuinely unresolved `CQ`/`DE` sitting right next to this
/// specific candidate (round 4 review, e.g. "DE W1AW CQ DX K5ARH T": the
/// resolved "DE W1AW" must not excuse the separate, unresolved "CQ DX"
/// immediately before "K5ARH T").
fn parse_power_step_beacons(
    text: &str,
    named_range: Option<&Range<usize>>,
) -> Vec<(String, SpotType, Range<usize>)> {
    let mut candidates = Vec::new();
    for caps in POWER_STEP_BEACON_RE.captures_iter(text) {
        let call_start = caps.get(1).unwrap().start();
        if let Some(filler_caps) = CQ_DE_IMMEDIATE_FILLER_RE.captures(&text[..call_start]) {
            // Group 1 excludes the `\s*` trailing whitespace the outer
            // match consumes up to the anchor -- comparing the whole
            // match's range against `named_range` would spuriously fail
            // containment even for an exact, legitimately-resolved match
            // like "DE W1AW" (`named_range` itself never includes a
            // trailing separator space).
            let filler = filler_caps.get(1).unwrap().range();
            let explained_by_named =
                named_range.is_some_and(|nr| nr.start <= filler.start && filler.end <= nr.end);
            if !explained_by_named {
                continue;
            }
        }
        let m = caps.get(0).unwrap();
        candidates.push((caps[1].to_uppercase(), SpotType::Beacon, m.range()));
    }
    candidates
}

/// Scans `text` for every CQ/DE/beacon context match, returning each as
/// (callsign candidate uppercased, spot type, byte range of every token
/// that determined that type). The named-keyword family
/// (`BEACON_RE`/`DE_RE`/`CQ_CALL_RE`/`UP_RE`) contributes at most one
/// candidate (first match wins within that family); the power-step
/// fallback (`POWER_STEP_BEACON_RE`) contributes one PER match it finds
/// (see `parse_power_step_beacons`). More than one candidate can come back
/// together -- e.g. an earlier "DE W1AW" and a later, unrelated "K5ARH T"
/// in the same rolling window both name real, independent transmission
/// fragments, and "W1AW T K5ARH T" names two independent power-step
/// occurrences.
///
/// The range lets a caller distinguish a genuine reclassification (driven
/// by a newly-arrived word) from a type merely changing because an older
/// framing word aged out of its own rolling window -- MAN-28 round 12
/// review: `manta-spot::Validator` maps each candidate's range back to
/// word identities and only accepts a reclassification when it covers a
/// word younger than any that produced the previous classification. This
/// is also what reconciles two candidates naming the SAME callsign (e.g.
/// "CQ K5ARH K5ARH T"): both map to the same decoded `Word`, and that
/// word's own seq-based provenance guard -- not a text-position heuristic
/// here -- decides whether the second one is a genuine reclassification.
/// `parse` itself does not try to pick a winner between candidates (Codex
/// review on PR #65, rounds 2-4: every attempt at a text-level
/// recency/same-callsign/"one match per family" heuristic here proved too
/// coarse in a new way every round); it just reports what it found and
/// lets the caller's real per-word state decide.
///
/// A `DE <call>` match is classified `Cq` (not `De`) when a bare `CQ` token
/// also appears anywhere in `text` -- the common "CQ CQ DE <call>"
/// transmission shape, where the callsign always follows `DE` but the
/// operator is calling CQ, not answering one.
pub fn parse(text: &str) -> Vec<(String, SpotType, Range<usize>)> {
    let named = parse_named_pattern(text);
    let named_range = named.as_ref().map(|(_, _, r)| r.clone());
    let mut candidates = Vec::new();
    if let Some(n) = named {
        candidates.push(n);
    }
    candidates.extend(parse_power_step_beacons(text, named_range.as_ref()));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts only the candidate/type of every returned match, ignoring
    /// byte ranges (exercised by their own tests below) and order.
    fn parse_types(text: &str) -> Vec<(String, SpotType)> {
        parse(text).into_iter().map(|(c, t, _)| (c, t)).collect()
    }

    #[test]
    fn cq_de_call_call_is_cq_type() {
        assert_eq!(
            parse_types("CQ CQ DE K5ARH K5ARH K"),
            vec![("K5ARH".to_string(), SpotType::Cq)]
        );
    }

    #[test]
    fn plain_de_call_without_cq_is_de_type() {
        assert_eq!(
            parse_types("DE K5ARH K"),
            vec![("K5ARH".to_string(), SpotType::De)]
        );
    }

    #[test]
    fn cq_test_call_is_cq_type() {
        assert_eq!(
            parse_types("CQ TEST K5ARH K5ARH"),
            vec![("K5ARH".to_string(), SpotType::Cq)]
        );
    }

    #[test]
    fn call_up_is_de_type() {
        assert_eq!(
            parse_types("K5ARH UP UP"),
            vec![("K5ARH".to_string(), SpotType::De)]
        );
    }

    #[test]
    fn v_v_v_call_is_beacon_type() {
        assert_eq!(
            parse_types("V V V K5ARH K5ARH"),
            vec![("K5ARH".to_string(), SpotType::Beacon)]
        );
    }

    #[test]
    fn no_pattern_returns_empty() {
        assert_eq!(parse("K5ARH TU 5NN"), vec![]);
    }

    #[test]
    fn de_call_range_covers_only_the_de_match_when_no_cq_present() {
        let text = "DE K5ARH K";
        let candidates = parse(text);
        assert_eq!(candidates.len(), 1);
        let (_, ty, range) = &candidates[0];
        assert_eq!(*ty, SpotType::De);
        assert_eq!(&text[range.clone()], "DE K5ARH");
    }

    #[test]
    fn cq_de_call_range_spans_both_the_cq_token_and_the_de_match() {
        // The CQ token here trails the DE match -- the range must extend
        // to cover it too, not just the leading DE-call span, since it's
        // what determines Cq over De (MAN-28 round 12).
        let text = "DE K5ARH CQ";
        let candidates = parse(text);
        assert_eq!(candidates.len(), 1);
        let (_, ty, range) = &candidates[0];
        assert_eq!(*ty, SpotType::Cq);
        assert_eq!(&text[range.clone()], "DE K5ARH CQ");
    }

    #[test]
    fn call_followed_by_lone_t_is_beacon_type() {
        // MAN-37: NCDXF/IARU beacons ID via a callsign followed by four
        // unmodulated power-step dashes, not a spoken "V V V" preamble.
        // The decoder can't resolve the individual power steps out of an
        // unbroken carrier (no inter-element keying gap to split on), so
        // the transmission decodes as the callsign followed by a single
        // trailing "T" word (one dash) -- not "TTTT".
        assert_eq!(
            parse_types("K5ARH T"),
            vec![("K5ARH".to_string(), SpotType::Beacon)]
        );
    }

    #[test]
    fn cq_call_followed_by_lone_t_yields_both_candidates() {
        // Both the CQ_CALL_RE match ("CQ K5ARH") and the power-step
        // fallback's match (the second "K5ARH T") are real, independent
        // evidence -- parse itself doesn't pick a winner (Validator's
        // per-word provenance does; see validator.rs's own golden vectors
        // for the end-to-end outcome).
        assert_eq!(
            parse_types("CQ K5ARH K5ARH T"),
            vec![
                ("K5ARH".to_string(), SpotType::Cq),
                ("K5ARH".to_string(), SpotType::Beacon),
            ]
        );
    }

    #[test]
    fn power_step_fallback_returns_every_distinct_occurrence() {
        // Codex review on PR #65, round 4: collapsing to a single "best"
        // match (first, last, or otherwise) can permanently lose a
        // never-yet-attempted occurrence -- e.g. two power-step IDs
        // decoding before the track's first TrackMeta, so neither got a
        // chance to be evaluated before both arrived. Both W1AW and
        // K5ARH are independent candidates; Validator's own per-word
        // attempted state (not this function) decides what actually
        // spots.
        assert_eq!(
            parse_types("W1AW T K5ARH T"),
            vec![
                ("W1AW".to_string(), SpotType::Beacon),
                ("K5ARH".to_string(), SpotType::Beacon),
            ]
        );
    }

    #[test]
    fn cq_dx_filler_call_followed_by_lone_t_is_not_beacon() {
        // Codex review on PR #65: "CQ DX <call>" doesn't match CQ_CALL_RE
        // (the "DX" filler breaks the tight adjacency pattern -- a known,
        // documented gap), but that must never let the power-step
        // fallback pick it up as Beacon just because the restricted CQ
        // pattern failed to match -- that would turn a merely-unrecognized
        // CQ call into one that wrongly bypasses the repetition gate.
        assert_eq!(parse_types("CQ DX K5ARH T"), vec![]);
    }

    #[test]
    fn cq_dx_repeated_call_followed_by_lone_t_is_not_beacon() {
        // Codex review on PR #65, round 5: a real CQ preamble commonly
        // repeats the callsign ("CQ DX K5ARH K5ARH"), putting TWO words
        // (the modifier "DX" and the first "K5ARH") between the bare "CQ"
        // and the candidate the fallback would tag -- the guard must
        // still recognize this as unresolved CQ framing, not just the
        // single-filler-word case.
        assert_eq!(parse_types("CQ DX K5ARH K5ARH T"), vec![]);
    }

    #[test]
    fn de_dx_filler_call_followed_by_lone_t_is_not_beacon() {
        // Same class of gap as the CQ case above, applied symmetrically:
        // "DE DX <call>" doesn't match DE_RE's tight adjacency pattern
        // either, and must not fall through to the power-step fallback.
        assert_eq!(parse_types("DE DX K5ARH T"), vec![]);
    }

    #[test]
    fn stale_unresolved_cq_several_words_earlier_does_not_block_a_newer_beacon() {
        // Codex review on PR #65, round 3: the unresolved-framing guard
        // must be scoped to the fallback's OWN candidate position, not the
        // whole window -- a "CQ DX" several words earlier (itself
        // unresolved, same as the case above) must not block a later,
        // unrelated "K5ARH T" that isn't part of that same utterance.
        assert_eq!(
            parse_types("CQ DX FILLER1 FILLER2 FILLER3 K5ARH T"),
            vec![("K5ARH".to_string(), SpotType::Beacon)]
        );
    }

    #[test]
    fn named_and_power_step_both_returned_for_different_callsigns() {
        // Codex review on PR #65, round 2: a named match earlier in the
        // window ("DE W1AW") must not preempt a genuinely newer, DIFFERENT
        // station's power-step evidence ("K5ARH T") -- these are two
        // independent transmission fragments, and Validator resolves each
        // against its own decoded Word.
        assert_eq!(
            parse_types("DE W1AW K5ARH T"),
            vec![
                ("W1AW".to_string(), SpotType::De),
                ("K5ARH".to_string(), SpotType::Beacon),
            ]
        );
    }

    #[test]
    fn unrelated_resolved_named_match_does_not_excuse_a_separate_unresolved_cq() {
        // Codex review on PR #65, round 4: the bare "CQ" later in this
        // text makes DE_RE's own pre-existing disambiguation classify
        // W1AW as `Cq` (not `De`) and widen its range to cover that "CQ"
        // token too (unrelated to MAN-37 -- see parse_named_pattern's own
        // docs) -- but that widened range still stops right after "CQ",
        // so it must NOT excuse the SEPARATE, still-unresolved "DX" filler
        // immediately before the power-step candidate. Checking "did any
        // named pattern match somewhere in the text" isn't enough; the
        // specific CQ/DE-plus-filler token adjacent to THIS candidate must
        // itself be fully covered by the named match's own range.
        assert_eq!(
            parse_types("DE W1AW CQ DX K5ARH T"),
            vec![("W1AW".to_string(), SpotType::Cq)]
        );
    }

    #[test]
    fn power_step_requires_t_to_be_a_complete_word() {
        // Codex review (round 2) on PR #65: the trailing `\b` only demands
        // a word/non-word transition, so it also matched "T/QRP" (a
        // portable-designator suffix glued onto the same decoded word) --
        // not a lone "T" word as the pattern is documented to require.
        assert_eq!(parse_types("K5ARH T/QRP"), vec![]);
    }
}
