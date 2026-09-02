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
static DE_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bDE\b").unwrap());
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
fn parse_power_step_beacons(text: &str) -> Vec<(String, SpotType, Range<usize>)> {
    POWER_STEP_BEACON_RE
        .captures_iter(text)
        .map(|caps| {
            let m = caps.get(0).unwrap();
            (caps[1].to_uppercase(), SpotType::Beacon, m.range())
        })
        .collect()
}

/// Scans `text` for every CQ/DE/beacon context match, returning each as
/// (callsign candidate uppercased, spot type, byte range of every token
/// that determined that type). The named-keyword family
/// (`BEACON_RE`/`DE_RE`/`CQ_CALL_RE`/`UP_RE`) contributes at most one
/// candidate (first match wins within that family); the power-step
/// fallback (`POWER_STEP_BEACON_RE`) contributes one PER match it finds
/// (see `parse_power_step_beacons`) -- but only when NEITHER a bare `CQ`
/// nor a bare `DE` token appears anywhere else in `text` at all.
///
/// That last rule is deliberately coarse, not position-scoped: a lone `T`
/// is also the single most common garbled/noise decode in CW, so without
/// SOME guard, an ordinary, unrecognized CQ/DE call (one whose own
/// adjacency-strict pattern merely failed to match, e.g. filler-word forms
/// like "CQ DX <call>") would get mistagged `Beacon` and wrongly bypass
/// the repetition gate. Three rounds of review (Codex review on PR #65,
/// rounds 3-6) tried increasingly precise position/range-based versions of
/// this guard -- scoped to the fallback's own vicinity, then to whether a
/// resolved named match's own range covered the nearby token, then to a
/// wider filler-word bound -- and each one only shifted the false-positive
/// gap to a new, subtler input shape (a resolved match elsewhere bridging
/// over genuinely unresolved content, a named match failing to cover a
/// SECOND resolved fragment, etc.). Given the real cost is asymmetric --
/// `Beacon` only lifts the repetition-gate requirement, it doesn't bypass
/// grammar/cty (ARCHITECTURE §6 step 4), and a real NCDXF beacon repeats
/// every ~10s, so a window suppressed by unrelated nearby CQ/DE chatter is
/// very likely followed by a clean one shortly after -- the simpler,
/// coarser rule was chosen deliberately over continuing to chase
/// text-position edge cases: occasionally missing a real beacon in a busy
/// window is a far smaller cost than reward-hacking a smarter-looking
/// heuristic that keeps finding new ways to be wrong.
///
/// The range lets a caller distinguish a genuine reclassification (driven
/// by a newly-arrived word) from a type merely changing because an older
/// framing word aged out of its own rolling window -- MAN-28 round 12
/// review: `manta-spot::Validator` maps each candidate's range back to
/// word identities and only accepts a reclassification when it covers a
/// word younger than any that produced the previous classification. This
/// is also what reconciles two candidates naming the SAME callsign (e.g.
/// "CQ K5ARH K5ARH", if a trailing "T" arrived without any bare CQ/DE
/// having already ruled the fallback out): both map to the same decoded
/// `Word`, and that word's own seq-based provenance guard -- not a
/// text-position heuristic here -- decides whether the second one is a
/// genuine reclassification. `parse` itself does not try to pick a winner
/// between candidates beyond the coarse guard above (Codex review on PR
/// #65, rounds 2-4: every attempt at a finer text-level
/// recency/same-callsign/"one match per family" heuristic here proved too
/// coarse in a new way every round); it just reports what it found and
/// lets the caller's real per-word state decide.
///
/// A `DE <call>` match is classified `Cq` (not `De`) when a bare `CQ` token
/// also appears anywhere in `text` -- the common "CQ CQ DE <call>"
/// transmission shape, where the callsign always follows `DE` but the
/// operator is calling CQ, not answering one.
pub fn parse(text: &str) -> Vec<(String, SpotType, Range<usize>)> {
    let mut candidates = Vec::new();
    if let Some(n) = parse_named_pattern(text) {
        candidates.push(n);
    }
    if !CQ_TOKEN_RE.is_match(text) && !DE_TOKEN_RE.is_match(text) {
        candidates.extend(parse_power_step_beacons(text));
    }
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
    fn any_bare_cq_anywhere_suppresses_the_power_step_fallback() {
        // Codex review on PR #65, rounds 3-6: after three consecutive
        // rounds of position/range-based refinements each traded one
        // false-positive shape for another, the guard was deliberately
        // simplified to a coarse, whole-window rule -- see parse's own
        // docs for the reasoning. "CQ DX K5ARH T" is the original round-1
        // motivating case: "DX" breaks CQ_CALL_RE's adjacency requirement,
        // but the bare "CQ" still exists in the window and now
        // unconditionally suppresses the fallback.
        assert_eq!(parse_types("CQ DX K5ARH T"), vec![]);
    }

    #[test]
    fn any_bare_de_anywhere_suppresses_the_power_step_fallback() {
        // Symmetric to the CQ case above.
        assert_eq!(parse_types("DE DX K5ARH T"), vec![]);
    }

    #[test]
    fn a_resolved_named_match_elsewhere_still_suppresses_the_fallback() {
        // Codex review on PR #65, round 6: this used to be a fix (a
        // resolved "DE W1AW" must not preempt a genuinely newer, DIFFERENT
        // station's power-step evidence), but the position/range-based
        // version of that fix is exactly what kept producing new
        // false-positive shapes every round. Under the coarse rule, the
        // bare "DE" suppresses K5ARH's candidate too, even though "DE
        // W1AW" itself resolved cleanly -- W1AW still spots on its own.
        assert_eq!(
            parse_types("DE W1AW K5ARH T"),
            vec![("W1AW".to_string(), SpotType::De)]
        );
    }

    #[test]
    fn a_stale_cq_several_words_earlier_still_suppresses_a_later_beacon() {
        // Codex review on PR #65, round 3's original refinement (scoping
        // the guard to the fallback's own vicinity) is deliberately
        // reverted along with the rest of the position-based guard -- see
        // parse's own docs. A real beacon repeats every ~10s, so an
        // occasional suppression here is an accepted trade for a much
        // simpler, more robust rule.
        assert_eq!(parse_types("CQ DX FILLER1 FILLER2 FILLER3 K5ARH T"), vec![]);
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
