//! Text-level context parse: a compatibility adapter over `exchange`, plus
//! the power-step beacon shape and its CQ/DE framing guard.
//! ARCHITECTURE §6.1.
//!
//! `exchange::interpret` decides who a callsign occurrence is (MAN-201);
//! `parse` reports the occurrences it reads as a sending station in a whole
//! decoded text. `Validator` runs the grammar over its own word window, not
//! through `parse`, so it can settle and latch each role per word. The
//! `<call> T` shape stays here: a lone `T` is not a grammar token, and the
//! guard below decides when the shape counts at all.

use crate::exchange::{self, ExchangeWord, Role};
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// The context a decoded callsign was found in. Carried on `Spot` as the
/// RBN spot-type flag (ARCHITECTURE §7).
///
/// JSON: `cq`, `de`, `beacon`, `unknown`, the live-state `spotType` and
/// `/metrics` `type` labels (MAN-139).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpotType {
    Cq,
    De,
    Beacon,
    Unknown,
}

impl std::fmt::Display for SpotType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Self::Cq => "CQ",
            Self::De => "DE",
            Self::Beacon => "BEACON",
            Self::Unknown => "unknown",
        })
    }
}

/// One `parse` match: callsign (uppercased), spot type, the byte range of
/// every word that decided it, and the exact range of the callsign word.
pub type ContextMatch = (String, SpotType, Range<usize>, Option<Range<usize>>);

/// Contest filler words skipped between a `CQ`/`TEST` framing token and the
/// call (MAN-104): "CQ WPX <call>", "CQ TEST CQ TEST <call>", "TEST DX
/// <call>". An enumerated list rather than "skip any digit-free word": a
/// generic rule would also skip a callsign garbled into a digit-free word and
/// capture the NEXT station instead. `CQ` is listed so a repeated "CQ WPX CQ
/// WPX <call>" resolves to the call.
///
/// Invariant: no entry contains a digit, so `grammar::is_plausible` rejects
/// every one (pinned by `no_filler_word_is_a_plausible_callsign`). Adding a
/// filler can therefore never create a spot by itself.
pub(crate) const CQ_FILLER: &str = "CQ|TEST|CONTEST|WPX|WW|NA|SS|FD|DX";

/// Sign-off words that make a following bare `TEST` the run station's
/// trailing "TU TEST" form, so the call after it is the next caller's, not
/// the CQer's (MAN-104; `exchange`).
pub(crate) const TEST_SIGN_OFF: [&str; 3] = ["TU", "TNX", "QSL"];

/// A bare `CQ`/`DE` framing token. The token must be a COMPLETE decoded word
/// in the joined window -- start-of-text or whitespace on both sides -- not
/// merely a `\b` word/non-word transition: `/` and `-` are themselves
/// non-word characters, so a plain `\bDE\b`/`\bCQ\b` also matched the two
/// letters glued inside ONE decoded word ("NOISE/DE W1AW T", "-CQ W1AW T").
/// That made the power-step guard below suppress -- and `Validator`
/// permanently burn -- a valid beacon occurrence on the strength of a garble
/// containing no framing token at all (MAN-48, deferred from Codex review on
/// PR #65, round 9). Same fix shape `POWER_STEP_BEACON_RE`'s trailing `T`
/// already carries from round 2.
///
/// The boundary is CONSUMED, not looked behind -- the `regex` crate has no
/// look-around. That widens a match by at most one space per side, which
/// cannot pull an extra word into a caller's overlap test: the widened bound
/// lands exactly ON the neighbouring word's own span boundary, and both
/// overlap tests in `Validator` are strict (`start < range.end && range.start
/// < end`).
static CQ_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|\s)CQ(?:\s|$)").unwrap());
static DE_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|\s)DE(?:\s|$)").unwrap());
/// A bare `TEST` token, under `CQ_TOKEN_RE`'s complete-word rule
/// (MAN-104). It suppresses the power-step fallback exactly as a bare `CQ`
/// does.
static TEST_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|\s)TEST(?:\s|$)").unwrap());
/// NCDXF/IARU-style beacon ID: a callsign followed by four unmodulated
/// power-step dashes (100W/10W/1W/100mW), no spoken "V V V" preamble
/// (MAN-37). The decoder's on/off-keying detector can't resolve the
/// individual power steps out of what is, to it, one unbroken carrier --
/// there's no inter-element keying gap to split the four dashes on -- so
/// this decodes as the callsign followed by a single trailing "T" (one
/// dash), not "TTTT". A lone "T" is also the single most common
/// garbled/noise decode in CW, so the shape alone is not beacon evidence
/// (MAN-201): only an allowlisted call becomes a `Beacon` candidate from it.
/// `parse` still reports the shape.
/// The trailing `T` must be a complete decoded word (end-of-text or
/// followed by whitespace), not just a word-boundary transition -- a bare
/// `\b` also matched a portable-designator suffix glued onto the same
/// word, e.g. "K5ARH T/QRP" (Codex review on PR #65).
static POWER_STEP_BEACON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Z0-9/]{3,15})\s+T(?:\s|$)").unwrap());

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
/// Returns, per match: the callsign (uppercased), the full match's byte
/// range (callsign + trailing "T", used to compute which words are
/// "involved" for the reclassification-aging guard, same as every other
/// pattern family), and the CALLSIGN CAPTURE'S OWN range alone. The
/// latter identifies the exact decoded word the callsign landed on --
/// `Validator` uses it to bind a match to that specific word rather than
/// searching for the newest word sharing its text, since two textually
/// identical but semantically unrelated power-step occurrences -- one
/// stale/suppressed, one a brand-new decode with no trailing "T" at all
/// -- can otherwise get conflated purely by sharing a callsign string
/// once the newer one evicts the framing that suppressed the older
/// (Codex review on PR #65, round 9).
fn parse_power_step_beacons(text: &str) -> Vec<(String, SpotType, Range<usize>, Range<usize>)> {
    POWER_STEP_BEACON_RE
        .captures_iter(text)
        .map(|caps| {
            let m = caps.get(0).unwrap();
            let call = caps.get(1).unwrap();
            (
                caps[1].to_uppercase(),
                SpotType::Beacon,
                m.range(),
                call.range(),
            )
        })
        .collect()
}

/// Every context match in `text`, read as a complete exchange: each
/// occurrence `exchange::interpret` calls `self_id` with a spot type, then
/// each `<call> T` power-step shape (see `parse_power_step_beacons`) -- but
/// only when NONE of a bare `CQ`, `TEST` or `DE` token appears anywhere in
/// `text` at all.
///
/// That guard is deliberately coarse, not position-scoped: a lone `T` is the
/// single most common garbled decode in CW, so an unrecognized CQ/DE call (a
/// garbled word in `CQ EE <call>`) would otherwise read as a beacon. Three
/// review rounds (PR #65, rounds 3-6) tried position- and range-scoped
/// versions, and each only shifted the false positive to a new input shape.
/// Missing a beacon in a busy window costs less than that, so the coarser
/// rule stands (MAN-37).
pub fn parse(text: &str) -> Vec<ContextMatch> {
    let mut ranges = Vec::new();
    let mut end = 0;
    for word in text.split_whitespace() {
        let start = end + text[end..].find(word).unwrap_or(0);
        end = start + word.len();
        ranges.push(start..end);
    }
    let words: Vec<ExchangeWord<'_>> = ranges
        .iter()
        .map(|r| ExchangeWord {
            text: &text[r.clone()],
            ends_turn: false,
            corrected: false,
        })
        .collect();
    let mut candidates: Vec<ContextMatch> = exchange::interpret(&words, true)
        .into_iter()
        .filter(|o| o.role == Role::SelfId)
        .filter_map(|o| {
            let spot_type = o.frame.and_then(exchange::Frame::spot_type)?;
            let first = ranges[o.span.start].start;
            let last = ranges[o.span.end - 1].end;
            Some((o.call, spot_type, first..last, Some(ranges[o.index].clone())))
        })
        .collect();
    if !power_step_framing_is_unresolved(text) {
        for (call, ty, range, call_range) in parse_power_step_beacons(text) {
            candidates.push((call, ty, range, Some(call_range)));
        }
    }
    candidates
}

/// Whether a bare `CQ`, `TEST` or `DE` token anywhere in `text` would
/// currently suppress the power-step fallback (see `parse`'s own docs).
pub fn power_step_framing_is_unresolved(text: &str) -> bool {
    CQ_TOKEN_RE.is_match(text) || TEST_TOKEN_RE.is_match(text) || DE_TOKEN_RE.is_match(text)
}

/// Every power-step match in `text`, ignoring the CQ/DE guard entirely --
/// i.e. exactly what `parse` would have returned from this family had the
/// guard not suppressed it. `Validator::candidates` uses this, gated on
/// `power_step_framing_is_unresolved`, to "burn" a suppressed candidate's
/// decoded word (mark it attempted, with no spot) even though `parse`
/// itself withheld it: without this, the suppression is silently undone
/// the moment the CQ/DE token ages out of the 16-word window, since a
/// withheld candidate never reaches `Validator` and so never marks
/// anything attempted -- letting the SAME, no-newer-evidence occurrence
/// spot later as if newly decoded (Codex review on PR #65, round 7).
///
/// Returns (callsign, full match range, callsign-only range) -- see
/// `parse`'s own docs for why the callsign-only range matters (round 9):
/// `Validator` uses it to burn the EXACT word the regex captured, not
/// whichever word currently shares that callsign's text.
pub fn power_step_candidates(text: &str) -> Vec<(String, Range<usize>, Range<usize>)> {
    parse_power_step_beacons(text)
        .into_iter()
        .map(|(call, _, range, call_range)| (call, range, call_range))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_labels_and_padding() {
        for (kind, label) in [
            (SpotType::Cq, "CQ"),
            (SpotType::De, "DE"),
            (SpotType::Beacon, "BEACON"),
            (SpotType::Unknown, "unknown"),
        ] {
            assert_eq!(kind.to_string(), label);
        }
        assert_eq!(format!("{:>4}", SpotType::Cq), "  CQ");
    }

    use super::*;

    /// Asserts only the candidate/type of every returned match, ignoring
    /// byte ranges (exercised by their own tests below) and order.
    fn parse_types(text: &str) -> Vec<(String, SpotType)> {
        parse(text).into_iter().map(|(c, t, _, _)| (c, t)).collect()
    }

    #[test]
    fn cq_de_call_call_is_cq_type() {
        assert_eq!(
            parse_types("CQ CQ DE K5ARH K5ARH K"),
            vec![
                ("K5ARH".to_string(), SpotType::Cq),
                ("K5ARH".to_string(), SpotType::Cq)
            ]
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
            vec![
                ("K5ARH".to_string(), SpotType::Cq),
                ("K5ARH".to_string(), SpotType::Cq)
            ]
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
            vec![
                ("K5ARH".to_string(), SpotType::Beacon),
                ("K5ARH".to_string(), SpotType::Beacon)
            ]
        );
    }

    #[test]
    fn no_pattern_returns_empty() {
        assert_eq!(parse("K5ARH TU 5NN"), vec![]);
    }

    #[test]
    fn de_call_range_covers_the_de_and_the_call_when_no_cq_present() {
        let text = "DE K5ARH K";
        let candidates = parse(text);
        assert_eq!(candidates.len(), 1);
        let (_, ty, range, exact) = &candidates[0];
        assert_eq!(*ty, SpotType::De);
        assert_eq!(&text[range.clone()], "DE K5ARH");
        assert_eq!(&text[exact.clone().unwrap()], "K5ARH");
    }

    #[test]
    fn cq_de_call_range_spans_both_the_cq_token_and_the_de_match() {
        // The CQ token here trails the DE match -- the range must extend
        // to cover it too, not just the leading DE-call span, since it's
        // what determines Cq over De (MAN-28 round 12).
        let text = "DE K5ARH CQ";
        let candidates = parse(text);
        assert_eq!(candidates.len(), 1);
        let (_, ty, range, _) = &candidates[0];
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
        // motivating case. Since MAN-104 "DX" is a contest filler word, so
        // CQ_CALL_RE resolves K5ARH as Cq, and the bare "CQ" still
        // unconditionally suppresses the fallback: no Beacon candidate.
        assert_eq!(
            parse_types("CQ DX K5ARH T"),
            vec![("K5ARH".to_string(), SpotType::Cq)]
        );
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
        // simpler, more robust rule. Since MAN-104 the named family skips
        // the "DX" filler and captures FILLER1 (grammar rejects it
        // downstream); what this test pins is that no K5ARH Beacon appears.
        assert_eq!(
            parse_types("CQ DX FILLER1 FILLER2 FILLER3 K5ARH T"),
            vec![("FILLER1".to_string(), SpotType::Cq)]
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

    #[test]
    fn power_step_candidates_ignores_the_cq_de_guard() {
        // Codex review on PR #65, round 7: Validator needs the raw match
        // even when `parse` itself withholds it, so it can burn the
        // suppressed word's attempted state and stop the guard's
        // suppression from being silently undone once the CQ/DE token
        // ages out of the window.
        assert!(power_step_framing_is_unresolved("CQ DX K5ARH T"));
        assert_eq!(
            power_step_candidates("CQ DX K5ARH T"),
            vec![("K5ARH".to_string(), 6..13, 6..11)]
        );
        assert!(!power_step_framing_is_unresolved("K5ARH T"));
    }

    #[test]
    fn cq_de_guard_requires_a_complete_decoded_word() {
        // MAN-48 (deferred from Codex review on PR #65, round 9): `\b` is only a
        // word/non-word transition, and `/`/`-` are themselves non-word
        // characters -- so `\bDE\b`/`\bCQ\b` also matched the two letters glued
        // inside ONE decoded word. A garbled "NOISE/DE" or "-CQ" is not the bare
        // framing token this guard is documented to look for, and suppressing on
        // it burns (permanently loses) a valid beacon occurrence. Same fix shape
        // POWER_STEP_BEACON_RE's trailing `T` already carries (round 2).
        assert!(!power_step_framing_is_unresolved("NOISE/DE W1AW T"));
        assert!(!power_step_framing_is_unresolved("-CQ W1AW T"));
        assert!(!power_step_framing_is_unresolved("K5ARH/QRP CQX W1AW T"));

        // A genuine bare token still suppresses, in every window position --
        // leading, middle, and trailing (the trailing case is what the `(?:\s|$)`
        // end-anchor buys; a plain `\s` would miss it).
        assert!(power_step_framing_is_unresolved("CQ DX K5ARH T"));
        assert!(power_step_framing_is_unresolved("W1AW DE K5ARH T"));
        assert!(power_step_framing_is_unresolved("K5ARH T DE"));
        assert!(power_step_framing_is_unresolved("K5ARH T CQ"));
    }

    #[test]
    fn a_cq_de_token_glued_to_punctuation_no_longer_suppresses_the_fallback() {
        // The end-to-end consequence of the guard fix at the `parse` layer.
        // "DE/NOISE"/"CQ/NOISE" are used rather than the finding's own
        // "NOISE/DE"/"-CQ" because DE_RE (`\bDE\s+<call>`) and CQ_CALL_RE
        // (`\bCQ...\s+<call>`) ALSO match those two, contributing a named
        // candidate that would muddy what this test is asserting. Trailing
        // punctuation breaks the named patterns' required `\s+` while still
        // tripping the old `\b`-based guard -- so this isolates the guard.
        assert_eq!(
            parse_types("DE/NOISE W1AW T"),
            vec![("W1AW".to_string(), SpotType::Beacon)]
        );
        assert_eq!(
            parse_types("CQ/NOISE W1AW T"),
            vec![("W1AW".to_string(), SpotType::Beacon)]
        );
    }

    #[test]
    fn de_call_is_not_reclassified_to_cq_by_a_glued_cq_substring() {
        // CQ_TOKEN_RE is shared with parse_named_pattern's De->Cq
        // reclassification (the "CQ CQ DE <call>" shape). A `CQ` glued inside one
        // decoded word is not a framing token for THAT purpose either, so the
        // fix deliberately applies to both call sites -- pinned here so the
        // shared-static decision is a tested choice, not an accident (MAN-48).
        assert_eq!(
            parse_types("DE K5ARH NOISE/CQ"),
            vec![("K5ARH".to_string(), SpotType::De)]
        );
        // ...while a genuine bare CQ still reclassifies, as before.
        assert_eq!(
            parse_types("CQ CQ DE K5ARH K5ARH K"),
            vec![
                ("K5ARH".to_string(), SpotType::Cq),
                ("K5ARH".to_string(), SpotType::Cq)
            ]
        );
    }

    #[test]
    fn accepted_power_step_candidate_carries_its_exact_call_word_range() {
        // Codex review on PR #65, round 9: Validator must bind a
        // power-step candidate to the EXACT word the regex captured, not
        // resolve it by text (unlike named patterns -- see parse's own
        // docs) -- otherwise a stale match's callsign can get attached to
        // an unrelated, brand-new same-text word once the original
        // suppressing framing ages out from under it.
        let candidates = parse("K5ARH T");
        assert_eq!(candidates.len(), 1);
        let (call, ty, full_range, exact) = &candidates[0];
        assert_eq!(call, "K5ARH");
        assert_eq!(*ty, SpotType::Beacon);
        assert_eq!(exact.clone(), Some(0..5));
        assert_eq!(&"K5ARH T"[full_range.clone()], "K5ARH T");
    }

    #[test]
    fn contest_cq_forms_classify_like_plain_cq() {
        // MAN-104: the ticket's three contest framings must classify exactly
        // like a plain "CQ W5AU". Before, "CQ WPX W5AU" captured WPX (which
        // grammar rejects) and "TEST W5AU" matched nothing, so neither spotted.
        let plain = parse_types("CQ W5AU");
        assert_eq!(plain, vec![("W5AU".to_string(), SpotType::Cq)]);
        for text in ["CQ WPX W5AU", "CQ TEST W5AU", "TEST W5AU"] {
            assert_eq!(parse_types(text), plain, "{text}");
        }
    }

    #[test]
    fn every_filler_word_is_skipped_between_cq_and_the_call() {
        // MAN-104: each enumerated filler word, alone or repeated, is skipped
        // and the call after it is captured.
        for filler in CQ_FILLER.split('|') {
            let text = format!("CQ {filler} W5AU");
            assert_eq!(
                parse_types(&text),
                vec![("W5AU".to_string(), SpotType::Cq)],
                "{text}"
            );
        }
        for text in [
            "CQ WPX TEST W5AU",
            "CQ WPX CQ WPX W5AU",
            "CQ TEST CQ TEST W5AU",
            "TEST TEST W5AU",
        ] {
            assert_eq!(
                parse_types(text),
                vec![("W5AU".to_string(), SpotType::Cq)],
                "{text}"
            );
        }
    }

    #[test]
    fn no_filler_word_is_a_plausible_callsign() {
        // MAN-104: the invariant CQ_FILLER's doc relies on. A filler that
        // grammar accepted could be skipped while being a real call, or spot
        // by itself when captured in a transient "CQ <filler>" window.
        for filler in CQ_FILLER.split('|') {
            assert!(!crate::grammar::is_plausible(filler), "{filler}");
        }
    }

    #[test]
    fn bare_test_between_two_different_calls_is_ambiguous() {
        // MAN-104: the run exchange "TU <mycall> TEST <caller>" puts the
        // NEXT caller's call right after TEST. Without the guard, K1ABC
        // spotted as Cq end-to-end.
        assert_eq!(parse_types("TU W5AU TEST K1ABC K1ABC"), vec![]);
        // The same call either side is a CQ loop, not an exchange...
        assert_eq!(
            parse_types("W5AU TEST W5AU"),
            vec![("W5AU".to_string(), SpotType::Cq)]
        );
        // ...and a preceding word grammar rejects (here "04") is no call at all.
        assert_eq!(
            parse_types("5NN 04 TEST W5AU"),
            vec![("W5AU".to_string(), SpotType::Cq)]
        );
    }

    #[test]
    fn bare_test_after_a_sign_off_word_is_ambiguous() {
        // MAN-104 (self-review): the short sign-off "TU TEST <caller>" also
        // puts the next caller right after TEST, and "TU" is no callsign, so
        // the different-call check alone let "TU TEST K1ABC K1ABC 5NN 05",
        // sent twice through Validator, spot K1ABC as Cq.
        for sign_off in TEST_SIGN_OFF {
            let text = format!("{sign_off} TEST K1ABC K1ABC 5NN 05");
            assert_eq!(parse_types(&text), vec![], "{text}");
        }
        // A sign-off before an earlier, leading "TEST <call>" changes nothing.
        assert_eq!(
            parse_types("TEST W5AU W5AU TU TEST K1ABC"),
            vec![
                ("W5AU".to_string(), SpotType::Cq),
                ("W5AU".to_string(), SpotType::Cq)
            ]
        );
    }

    #[test]
    fn bare_test_must_be_a_complete_decoded_word() {
        // MAN-104: same complete-word rule as CQ_TOKEN_RE (MAN-48). A `\b`
        // anchor matched the TEST glued inside "K5ARH/TEST".
        assert_eq!(parse_types("K5ARH/TEST W5AU"), vec![]);
    }

    #[test]
    fn test_token_acts_like_cq_for_de_promotion_and_the_power_step_guard() {
        // MAN-104: a bare TEST promotes the "DE <call>" it precedes and
        // suppresses the power-step fallback, as a bare CQ does.
        assert_eq!(
            parse_types("TEST DE W5AU"),
            vec![("W5AU".to_string(), SpotType::Cq)]
        );
        assert_eq!(
            parse_types("TEST W5AU T"),
            vec![("W5AU".to_string(), SpotType::Cq)]
        );
        assert!(power_step_framing_is_unresolved("TEST K5ARH T"));
        assert!(!power_step_framing_is_unresolved("CONTEST K5ARH T"));
    }

    #[test]
    fn sign_off_test_does_not_promote_the_next_callers_de() {
        // MAN-104 (Codex review on PR #239): DE_RE runs before TEST_CALL_RE,
        // so a run station's trailing "TU TEST" / "TU <mycall> TEST" ahead of
        // the next caller's "DE <call>" promoted that caller to Cq. Such a
        // TEST gets the same guard as TEST_CALL_RE's, and a TEST that does
        // not directly precede the DE match (an older CQ, or a later CQer's
        // own "TEST DE") promotes nothing: the caller stays De.
        for text in [
            "TU TEST DE K1ABC",
            "TU W5AU TEST DE K1ABC",
            "TU TEST TEST DE K1ABC",
            "TEST TU TEST DE K1ABC",
        ] {
            assert_eq!(
                parse_types(text),
                vec![("K1ABC".to_string(), SpotType::De)],
                "{text}"
            );
        }
        // MAN-201: every sending occurrence is reported, not only the first.
        assert_eq!(
            parse_types("TEST W5AU W5AU DE K1ABC"),
            vec![
                ("W5AU".to_string(), SpotType::Cq),
                ("K1ABC".to_string(), SpotType::De)
            ]
        );
        assert_eq!(
            parse_types("TU TEST DE K1ABC K1ABC TEST DE W5AU"),
            vec![
                ("K1ABC".to_string(), SpotType::De),
                ("K1ABC".to_string(), SpotType::De),
                ("W5AU".to_string(), SpotType::De)
            ]
        );
        // A TEST the guard accepts still promotes, as does a bare CQ after
        // a rejected TEST.
        for text in ["W5AU TEST DE W5AU", "TU TEST CQ DE K1ABC"] {
            let call = text.rsplit(' ').next().unwrap().to_string();
            assert_eq!(parse_types(text), vec![(call, SpotType::Cq)], "{text}");
        }
    }
}
