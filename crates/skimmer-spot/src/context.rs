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
/// callsign candidate (uppercased) and its spot type. `None` if no pattern
/// matches at all -- the caller decides whether to fall back to
/// grammar-only, type-`Unknown` validation.
///
/// A `DE <call>` match is classified `Cq` (not `De`) when a bare `CQ` token
/// also appears anywhere in `text` -- the common "CQ CQ DE <call>"
/// transmission shape, where the callsign always follows `DE` but the
/// operator is calling CQ, not answering one.
pub fn parse(text: &str) -> Option<(String, SpotType)> {
    if let Some(caps) = BEACON_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::Beacon));
    }
    if let Some(caps) = DE_RE.captures(text) {
        let spot_type = if CQ_TOKEN_RE.is_match(text) {
            SpotType::Cq
        } else {
            SpotType::De
        };
        return Some((caps[1].to_uppercase(), spot_type));
    }
    if let Some(caps) = CQ_CALL_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::Cq));
    }
    if let Some(caps) = UP_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::De));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cq_de_call_call_is_cq_type() {
        assert_eq!(
            parse("CQ CQ DE K5ARH K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn plain_de_call_without_cq_is_de_type() {
        assert_eq!(
            parse("DE K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn cq_test_call_is_cq_type() {
        assert_eq!(
            parse("CQ TEST K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn call_up_is_de_type() {
        assert_eq!(
            parse("K5ARH UP UP"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn v_v_v_call_is_beacon_type() {
        assert_eq!(
            parse("V V V K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Beacon))
        );
    }

    #[test]
    fn no_pattern_returns_none() {
        assert_eq!(parse("K5ARH TU 5NN"), None);
    }
}
