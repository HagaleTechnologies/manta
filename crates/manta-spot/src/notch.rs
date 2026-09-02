//! Operator-maintained notched-frequency list (MAN-31): fixed ranges that
//! persistently generate false spots (birdies, spurs, local noise
//! sources). Orthogonal to the automatic validation pipeline
//! (ARCHITECTURE §6 steps 1-4). Legacy precedent: Aggregator's "Notched
//! Frequencies File".

/// A closed frequency interval `[low_hz, high_hz]`, inclusive of both
/// endpoints so an operator can notch an exact spur frequency with a
/// zero-width range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqRange {
    pub low_hz: f64,
    pub high_hz: f64,
}

impl FreqRange {
    pub fn contains(&self, freq_hz: f64) -> bool {
        freq_hz >= self.low_hz && freq_hz <= self.high_hz
    }
}

/// A pure boolean membership lookup over operator-notched frequency
/// ranges -- like `Blocklist`, this cannot affect `Spot` output ordering
/// (SPEC-decode-core.md §6 rule 3), so a plain `Vec` is fine.
#[derive(Debug, Default, Clone)]
pub struct NotchList {
    ranges: Vec<FreqRange>,
}

impl NotchList {
    /// Parses one `low_hz-high_hz` range per line; blank lines and
    /// `#`-prefixed comment lines are ignored (legacy Aggregator "Notched
    /// Frequencies File" format). Malformed lines are ignored.
    pub fn parse(text: &str) -> Self {
        let ranges = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| {
                let (lo, hi) = line.split_once('-')?;
                let lo: f64 = lo.trim().parse().ok()?;
                let hi: f64 = hi.trim().parse().ok()?;
                Some(FreqRange {
                    low_hz: lo.min(hi),
                    high_hz: lo.max(hi),
                })
            })
            .collect();
        Self { ranges }
    }

    pub fn contains(&self, freq_hz: f64) -> bool {
        self.ranges.iter().any(|r| r.contains(freq_hz))
    }

    #[cfg(test)]
    fn ranges(&self) -> &[FreqRange] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# local birdie
14025000-14025050

7040100.5-7040100.5
";

    #[test]
    fn frequency_inside_a_range_is_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(14_025_025.0));
    }

    #[test]
    fn frequency_at_a_range_boundary_is_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(14_025_000.0));
        assert!(notch.contains(14_025_050.0));
    }

    #[test]
    fn zero_width_range_notches_exactly_one_frequency() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(7_040_100.5));
        assert!(!notch.contains(7_040_101.0));
    }

    #[test]
    fn frequency_outside_every_range_is_not_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(!notch.contains(14_030_000.0));
    }

    #[test]
    fn comment_and_blank_lines_are_ignored() {
        let notch = NotchList::parse(FIXTURE);
        assert_eq!(notch.ranges().len(), 2);
    }
}
