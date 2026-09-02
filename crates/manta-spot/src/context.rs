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
/// dash), not "TTTT". Checked only as a last-resort fallback, after every
/// other named-keyword pattern: a lone "T" is also the single most common
/// garbled/noise decode in CW, so this pattern is a known, accepted
/// source of false-positive `Beacon` tags (see MAN-37 decision notes) --
/// bounded blast radius since `Beacon` only lifts the repetition-gate
/// requirement (ARCHITECTURE §6 step 4), it doesn't bypass grammar/cty.
/// Never fires when a bare `CQ` or `DE` token appears anywhere in `text`,
/// even if the adjacency-strict `CQ_CALL_RE`/`DE_RE` patterns themselves
/// failed to match (e.g. filler-word forms like "CQ DX <call>") -- an
/// explicit framing keyword must never be reinterpreted as Beacon just
/// because its own restricted pattern didn't fire (Codex review on PR #65).
static POWER_STEP_BEACON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Z0-9/]{3,15})\s+T\b").unwrap());
/// Bare `DE` token, analogous to `CQ_TOKEN_RE` -- used only to keep the
/// power-step fallback from firing over explicit `DE` framing that
/// `DE_RE` itself failed to match due to filler words (mirrors the
/// `CQ_TOKEN_RE` guard below; MAN-37 review).
static DE_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bDE\b").unwrap());

/// Scans `text` for the first CQ/DE/beacon context pattern, returning the
/// callsign candidate (uppercased), its spot type, and the byte range in
/// `text` of every token that determined that type (not just the capture
/// group -- the full pattern match, plus the `CQ` token's own span when it
/// decides the `De`-vs-`Cq` ambiguity below). `None` if no pattern matches
/// at all -- the caller decides whether to fall back to grammar-only,
/// type-`Unknown` validation.
///
/// The range lets a caller distinguish a genuine reclassification (driven
/// by a newly-arrived word) from a type merely changing because an older
/// framing word aged out of its own rolling window -- MAN-28 round 12
/// review: `manta-spot::Validator` maps this range back to word identities
/// and only accepts a reclassification when it covers a word younger than
/// any that produced the previous classification.
///
/// A `DE <call>` match is classified `Cq` (not `De`) when a bare `CQ` token
/// also appears anywhere in `text` -- the common "CQ CQ DE <call>"
/// transmission shape, where the callsign always follows `DE` but the
/// operator is calling CQ, not answering one.
pub fn parse(text: &str) -> Option<(String, SpotType, std::ops::Range<usize>)> {
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
    if !CQ_TOKEN_RE.is_match(text) && !DE_TOKEN_RE.is_match(text) {
        // `captures` alone only ever returns the FIRST "<call> T" match in
        // `text`; with the 16-word rolling window this fallback actually
        // runs against, an older, already-attempted occurrence sitting
        // earlier in the window must not starve a newer, genuinely valid
        // one -- take the last (newest) match instead (Codex review on
        // PR #65).
        if let Some(caps) = POWER_STEP_BEACON_RE.captures_iter(text).last() {
            let m = caps.get(0).unwrap();
            return Some((caps[1].to_uppercase(), SpotType::Beacon, m.range()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts only the candidate/type; the byte range is exercised by
    /// its own tests below (it's an internal reclassification-tracking
    /// detail, not part of the pattern-family contract this test covers).
    fn parse_type(text: &str) -> Option<(String, SpotType)> {
        parse(text).map(|(call, ty, _range)| (call, ty))
    }

    #[test]
    fn cq_de_call_call_is_cq_type() {
        assert_eq!(
            parse_type("CQ CQ DE K5ARH K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn plain_de_call_without_cq_is_de_type() {
        assert_eq!(
            parse_type("DE K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn cq_test_call_is_cq_type() {
        assert_eq!(
            parse_type("CQ TEST K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn call_up_is_de_type() {
        assert_eq!(
            parse_type("K5ARH UP UP"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn v_v_v_call_is_beacon_type() {
        assert_eq!(
            parse_type("V V V K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Beacon))
        );
    }

    #[test]
    fn no_pattern_returns_none() {
        assert_eq!(parse("K5ARH TU 5NN"), None);
    }

    #[test]
    fn de_call_range_covers_only_the_de_match_when_no_cq_present() {
        let text = "DE K5ARH K";
        let (_, ty, range) = parse(text).unwrap();
        assert_eq!(ty, SpotType::De);
        assert_eq!(&text[range], "DE K5ARH");
    }

    #[test]
    fn cq_de_call_range_spans_both_the_cq_token_and_the_de_match() {
        // The CQ token here trails the DE match -- the range must extend
        // to cover it too, not just the leading DE-call span, since it's
        // what determines Cq over De (MAN-28 round 12).
        let text = "DE K5ARH CQ";
        let (_, ty, range) = parse(text).unwrap();
        assert_eq!(ty, SpotType::Cq);
        assert_eq!(&text[range], "DE K5ARH CQ");
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
            parse_type("K5ARH T"),
            Some(("K5ARH".to_string(), SpotType::Beacon))
        );
    }

    #[test]
    fn cq_call_followed_by_lone_t_stays_cq_type() {
        // The power-step fallback must not override an explicit CQ/DE/UP
        // framing keyword just because a trailing "T" also happens to
        // follow the callsign somewhere in the window.
        assert_eq!(
            parse_type("CQ K5ARH K5ARH T"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn power_step_fallback_picks_the_newest_call_t_occurrence() {
        // Codex review on PR #65: `captures` on its own only ever returns
        // the FIRST "<call> T" match in the window. Once an older one
        // (already attempted/rejected) is sitting earlier in the window,
        // a newer, genuinely valid beacon occurrence must not be starved
        // by it -- the fallback has to pick the newest match, not the
        // oldest.
        assert_eq!(
            parse_type("W1AW T K5ARH T"),
            Some(("K5ARH".to_string(), SpotType::Beacon))
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
        assert_eq!(parse_type("CQ DX K5ARH T"), None);
    }

    #[test]
    fn de_dx_filler_call_followed_by_lone_t_is_not_beacon() {
        // Same class of gap as the CQ case above, applied symmetrically:
        // "DE DX <call>" doesn't match DE_RE's tight adjacency pattern
        // either, and must not fall through to the power-step fallback.
        assert_eq!(parse_type("DE DX K5ARH T"), None);
    }
}
