//! Operator-maintained bad-callsign blocklist (MAN-31). Orthogonal to the
//! automatic validation pipeline (ARCHITECTURE §6 steps 1-4) -- a manual
//! override for known-bad callsigns automatic validation doesn't catch.
//! Legacy precedent: Aggregator's "Bad Calls File".

use std::collections::HashSet;

/// A pure boolean membership lookup, same documented `HashSet` exception
/// as `scp::Set` (SPEC-decode-core.md §6 rule 3) -- it cannot affect
/// `Spot` output ordering.
#[derive(Debug, Default, Clone)]
pub struct Blocklist {
    calls: HashSet<String>,
}

impl Blocklist {
    /// Parses one callsign per line; blank lines and `#`-prefixed comment
    /// lines are ignored (legacy Aggregator "Bad Calls File" format).
    pub fn parse(text: &str) -> Self {
        let calls = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_uppercase)
            .collect();
        Self { calls }
    }

    pub fn contains(&self, callsign: &str) -> bool {
        self.calls.contains(&callsign.to_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# operator bad-call list
K1BAD

w3fake
";

    #[test]
    fn listed_calls_are_found() {
        let list = Blocklist::parse(FIXTURE);
        assert!(list.contains("K1BAD"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let list = Blocklist::parse(FIXTURE);
        assert!(list.contains("w3fake"));
        assert!(list.contains("W3FAKE"));
    }

    #[test]
    fn unlisted_calls_are_absent() {
        let list = Blocklist::parse(FIXTURE);
        assert!(!list.contains("K5ARH"));
    }

    #[test]
    fn comment_and_blank_lines_are_not_members() {
        let list = Blocklist::parse(FIXTURE);
        assert!(!list.contains("# operator bad-call list"));
        assert!(!list.contains(""));
    }
}
