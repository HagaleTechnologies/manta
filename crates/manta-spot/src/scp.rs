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

impl Set {
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
