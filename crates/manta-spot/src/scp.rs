//! master.scp (Super Check Partial) membership. ARCHITECTURE §6.3.
//!
//! Format: one callsign per line; `#`/`!!`-prefixed lines are
//! comments/headers. `HashSet` here is the one documented exception to
//! this crate's "no `HashMap`/`HashSet` on an output-ordering path" rule
//! (SPEC-decode-core.md §6 rule 3) -- membership is a pure boolean lookup
//! that cannot affect `Spot` output ordering.

use std::collections::HashSet;

pub struct Set {
    calls: HashSet<String>,
}

impl std::fmt::Debug for Set {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Set").field("calls", &self.len()).finish()
    }
}

impl Set {
    /// The table built into manta.
    pub fn bundled() -> Self {
        Self::parse(crate::MASTER_SCP)
    }
    /// Number of distinct calls in the table.
    pub fn len(&self) -> usize {
        self.calls.len()
    }
    /// Whether the table has no calls.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Parses a `MASTER.SCP` file's full contents.
    pub fn parse(master_scp: &str) -> Self {
        let calls = master_scp
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
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

    #[test]
    fn len_counts_calls_not_headers() {
        assert_eq!(Set::parse(FIXTURE).len(), 3);
    }
    #[test]
    fn comment_only_input_is_empty() {
        assert!(Set::parse("!!header\n# comment\n").is_empty());
    }
    #[test]
    fn debug_summarises_instead_of_dumping_the_set() {
        let text = format!("{:?}", Set::parse(crate::MASTER_SCP));
        assert!(text.starts_with("Set { calls: "), "{text}");
        assert!(text.len() < 64, "{text}");
    }
    #[test]
    fn bundled_is_the_vendored_file() {
        assert_eq!(Set::bundled().len(), Set::parse(crate::MASTER_SCP).len());
    }

    const FIXTURE: &str = "\
!!Order,1,1
#
# Super Check Partial
# Release 2026.07.24
#
K5ARH
W1AW
VE3ABC
";

    #[test]
    fn member_calls_are_found() {
        let scp = Set::parse(FIXTURE);
        assert!(scp.contains("K5ARH"));
        assert!(scp.contains("w1aw")); // case-insensitive
    }

    #[test]
    fn non_member_calls_are_absent() {
        let scp = Set::parse(FIXTURE);
        assert!(!scp.contains("ZZ9ZZZ"));
    }

    #[test]
    fn header_and_comment_lines_are_not_members() {
        let scp = Set::parse(FIXTURE);
        assert!(!scp.contains("!!Order,1,1"));
        assert!(!scp.contains("#"));
    }
}
