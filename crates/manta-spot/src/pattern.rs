//! Callsign-pattern rarity and the repetition thresholds it controls (MAN-95).
//!
//! Like `scp::Set`, the `HashMap` is a pure lookup: its iteration order
//! cannot affect spots. Generation uses ordered collections for stable bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A callsign pattern's frequency in the reference list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternTier {
    Common,
    Listed,
    Unlisted,
}

impl PatternTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Listed => "listed",
            Self::Unlisted => "unlisted",
        }
    }
}

/// Repetition policy for ordinary candidates. Exemptions are handled by the validator.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    Minimal,
    #[default]
    Normal,
}

impl ValidationLevel {
    pub const fn required_reps(self, tier: PatternTier) -> u32 {
        match (self, tier) {
            (Self::Minimal, _) | (Self::Normal, PatternTier::Common) => 2,
            (Self::Normal, PatternTier::Listed) => 3,
            (Self::Normal, PatternTier::Unlisted) => 5,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Normal => "normal",
        }
    }
}

/// Pattern of the base call: retain 3 characters, or 2 for letter-digit-letter,
/// or 4 for digit-letter-letter; replace remaining letters/digits with `@`/`#`.
/// The admission grammar runs separately; this function also accepts short input.
pub fn pattern_of(call: &str) -> String {
    let base = call.split('/').next().unwrap_or_default();
    let chars: Vec<char> = base.chars().map(|c| c.to_ascii_uppercase()).collect();
    let keep = match chars.as_slice() {
        [a, b, c, ..]
            if a.is_ascii_alphabetic() && b.is_ascii_digit() && c.is_ascii_alphabetic() =>
        {
            2
        }
        [a, b, c, ..]
            if a.is_ascii_digit() && b.is_ascii_alphabetic() && c.is_ascii_alphabetic() =>
        {
            4
        }
        _ => 3,
    };
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            if i < keep {
                c
            } else if c.is_ascii_alphabetic() {
                '@'
            } else if c.is_ascii_digit() {
                '#'
            } else {
                c
            }
        })
        .collect()
}

/// A malformed line or an empty operator pattern list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    InvalidLine { line: usize, text: String },
    Empty,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLine { line, text } => write!(
                f,
                "line {line}: `{text}` is not a pattern ([+]PATTERN, for example +K5@@@)"
            ),
            Self::Empty => f.write_str("contains no patterns"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A reference list in `[+]PATTERN` format. `+` marks common patterns.
pub struct Table {
    patterns: HashMap<String, bool>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table")
            .field("patterns", &self.len())
            .field("common", &self.common_len())
            .finish()
    }
}

impl Table {
    pub fn bundled() -> Self {
        Self::parse(crate::PATTERNS_LST).expect("vendored patterns.lst is valid")
    }

    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut patterns = HashMap::new();
        for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (common, pattern) = match line.strip_prefix('+') {
                Some(pattern) => (true, pattern.trim_start()),
                None => (false, line),
            };
            // A valid entry is exactly the canonical shape of a plausible call.
            // Substitution preserves the prefix-shape rule while detecting literal
            // suffix letters, misplaced wildcards, and non-ASCII input.
            let call = pattern.replace('@', "A").replace('#', "0");
            if !crate::grammar::is_plausible(&call) || pattern_of(&call) != pattern {
                return Err(ParseError::InvalidLine {
                    line: index + 1,
                    text: line.to_owned(),
                });
            }
            patterns
                .entry(pattern.to_owned())
                .and_modify(|value| *value |= common)
                .or_insert(common);
        }
        let table = Self { patterns };
        if table.is_empty() {
            Err(ParseError::Empty)
        } else {
            Ok(table)
        }
    }

    pub fn tier(&self, call: &str) -> PatternTier {
        match self.patterns.get(&pattern_of(call)) {
            Some(true) => PatternTier::Common,
            Some(false) => PatternTier::Listed,
            None => PatternTier::Unlisted,
        }
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }
    pub fn common_len(&self) -> usize {
        self.patterns.values().filter(|common| **common).count()
    }
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Derive a deterministic independent reference list from distinct plausible SCP calls.
/// Two or more calls sharing a pattern make it common; singletons remain listed.
pub fn derive_list(master_scp: &str) -> String {
    let calls: BTreeSet<_> = crate::scp::calls(master_scp)
        .filter(|call| crate::grammar::is_plausible(call))
        .collect();
    let mut counts = BTreeMap::<String, usize>::new();
    for call in calls {
        *counts.entry(pattern_of(&call)).or_default() += 1;
    }
    let mut result = String::from("# MAN-95: independent callsign patterns derived from MASTER.SCP\n# Generate: cargo run -p manta-spot --example gen_pattern_list\n");
    if let Some(release) = master_scp
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("# Release"))
    {
        result.push_str(release);
        result.push('\n');
    }
    result.push_str("# +PATTERN: at least two SCP calls; PATTERN: one call; absent: unlisted\n");
    for (pattern, count) in counts {
        if count >= 2 {
            result.push('+');
        }
        result.push_str(&pattern);
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_of_documented_examples() {
        for (call, expected) in [
            ("K5ARH", "K5@@@"),
            ("N4ZR", "N4@@"),
            ("3DA0RU", "3DA0@@"),
            ("VE3ABC", "VE3@@@"),
            ("2E0AOZ", "2E0@@@"),
            ("AA1A", "AA1@"),
            ("K1N", "K1@"),
            ("3DA", "3DA"),
            ("K5ARH/P", "K5@@@"),
            ("k5arh", "K5@@@"),
            ("A12B3C", "A12@#@"),
        ] {
            assert_eq!(pattern_of(call), expected, "{call}");
        }
        for call in ["", "A", "é🦀A", "🦀é"] {
            let _ = pattern_of(call);
        }
    }

    #[test]
    fn parse_accepts_documented_syntax_and_common_wins_duplicates() {
        let table =
            Table::parse("\u{feff}# header\r\n\r\nK5@@@\r\n+ K5@@@\r\n+K5@@@\nK5@@@\nN8@\n")
                .unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table.common_len(), 1);
        assert!(!table.is_empty());
        assert_eq!(table.tier("K5ARH/P"), PatternTier::Common);
        assert_eq!(table.tier("N8A"), PatternTier::Listed);
        assert_eq!(table.tier("W4KTNL"), PatternTier::Unlisted);
    }

    #[test]
    fn parse_rejects_invalid_lines_with_line_number() {
        for bad in [
            "K5ARH;", "+", "K5ARH", "K5@@A", "K5##", "ABC", "K5@@@/P", "k5@@@", "é@@", "@@@", "999",
        ] {
            let error = Table::parse(&format!("# header\n{bad}\n"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("line 2:"), "{bad}: {error}");
            assert!(error.contains("is not a pattern"), "{error}");
        }
    }

    #[test]
    fn parse_refuses_empty_or_comment_only_file() {
        for text in ["", "\n# comments\n", "\u{feff}# comment\r\n"] {
            assert_eq!(
                Table::parse(text).unwrap_err().to_string(),
                "contains no patterns"
            );
        }
    }

    #[test]
    fn levels_require_two_three_five_or_flat_two() {
        assert_eq!(ValidationLevel::default(), ValidationLevel::Normal);
        for (tier, normal, name) in [
            (PatternTier::Common, 2, "common"),
            (PatternTier::Listed, 3, "listed"),
            (PatternTier::Unlisted, 5, "unlisted"),
        ] {
            assert_eq!(ValidationLevel::Normal.required_reps(tier), normal);
            assert_eq!(ValidationLevel::Minimal.required_reps(tier), 2);
            assert_eq!(tier.as_str(), name);
        }
        assert_eq!(ValidationLevel::Normal.as_str(), "normal");
        assert_eq!(ValidationLevel::Minimal.as_str(), "minimal");
    }

    #[test]
    fn derive_list_is_sorted_and_counts_distinct_plausible_calls() {
        let text = derive_list("!!header\n# Release test\nN8A\nK5ARH\nK5ABC\nN8A\n5NN\nINVALID\n");
        assert!(text.starts_with("# MAN-95"));
        assert!(text.contains("# Release test\n"));
        let rows: Vec<_> = text.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(rows, ["+K5@@@", "N8@"]);
    }

    #[test]
    fn vendored_list_is_derived_from_the_vendored_master_scp() {
        assert_eq!(derive_list(crate::MASTER_SCP), crate::PATTERNS_LST);
    }

    #[test]
    fn bundled_table_has_the_measured_shape() {
        let table = Table::bundled();
        assert_eq!(table.len(), 6036);
        assert_eq!(table.common_len(), 4018);
        assert!(format!("{table:?}").len() < 80);
    }
}
