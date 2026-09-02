//! CQ/DE/beacon context parse. ARCHITECTURE §6.1.
//!
//! Deliberately lightweight: matches the exact pattern families
//! ARCHITECTURE §6.1 lists (`CQ <call>`, `CQ TEST <call>`, `DE <call>`,
//! `<call> UP`, `V V V <call>`). Filler words between the keyword and the
//! call (e.g. "CQ DX CQ DX DE ...", "CQ CONTEST ...") are a known gap, not
//! handled by this first pass -- same "tracked, not blocking" treatment
//! this project gives other classical-parsing limitations (see the known
//! decode bugs tracked as GitHub issues).

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
}
