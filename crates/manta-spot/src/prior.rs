//! Bounded word evidence. Lists score existing readings, never generate them.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::reference::{strip_bom, Live, ReferenceLists};
use manta_decode::prior::WordPrior;

/// Hearing counts from an optional operator-maintained file.
#[derive(Debug, Default)]
pub struct CallsHeard(pub HashMap<String, u64>);

impl CallsHeard {
    pub fn parse(text: &str) -> Self {
        let mut counts = HashMap::<String, u64>::new();
        for line in crate::scp::calls(strip_bom(text)) {
            let mut fields = line.split_whitespace();
            let Some(call) = fields.next() else { continue };
            if !call.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'/') {
                continue;
            }
            let count = match fields.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => continue,
                },
                None => 1,
            };
            if fields.next().is_some() {
                continue;
            }
            let total = counts.entry(call.to_string()).or_default();
            *total = total.saturating_add(count);
        }
        Self(counts)
    }

    /// An absent file supplies no evidence; other I/O errors remain visible.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug)]
pub struct LanguagePrior {
    lists: Arc<ReferenceLists>,
    calls_heard: Live<CallsHeard>,
    cap: f32,
}

impl LanguagePrior {
    pub fn new(lists: Arc<ReferenceLists>, calls_heard: CallsHeard, cap: f32) -> Self {
        Self {
            lists,
            calls_heard: Live::new(Arc::new(calls_heard)),
            cap,
        }
    }
}

const WORDS: &[&str] = &[
    "CQ", "DE", "5NN", "599", "TNX", "PSE", "TU", "QRZ", "GL", "GE", "GM", "GA", "AGN", "QTH",
    "NAME", "RST", "73", "BK", "TEST", "QRL", "QRS", "QRO", "QRP", "ES", "FER", "UR", "HR", "WX",
    "ANT", "RIG",
];

impl WordPrior for LanguagePrior {
    fn reading_bonus(&self, reading: &str) -> f32 {
        // Each component has a fixed share; the decoder also enforces its cap.
        let scp = self.lists.scp.load();
        let cty = self.lists.cty.load();
        let heard = self.calls_heard.load();
        let membership = if scp.contains(reading) { 0.35 } else { 0.0 };
        let frequency = heard.0.get(reading).map_or(0.0, |&n| {
            ((n as f32).ln_1p() / 1001.0_f32.ln()).min(1.0) * 0.45
        });
        let prefix = if cty.is_allocated(reading) { 0.10 } else { 0.0 };
        let word = if WORDS.contains(&reading) { 0.10 } else { 0.0 };
        ((membership + frequency + prefix + word) * self.cap).clamp(0.0, self.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cty, scp};
    fn prior(scp: &str, heard: &str) -> LanguagePrior {
        LanguagePrior::new(
            Arc::new(ReferenceLists::new(
                Arc::new(cty::Table::parse("")),
                Arc::new(scp::Set::parse(scp)),
            )),
            CallsHeard::parse(heard),
            2.0,
        )
    }
    #[test]
    fn scp_member_bonus_is_bounded_and_unknown_is_zero() {
        let p = prior("W1AW", "");
        assert_eq!(p.reading_bonus("W1AW"), 0.7);
        assert_eq!(p.reading_bonus("AB2KLK"), 0.0);
    }
    #[test]
    fn calls_heard_bonus_scales_with_log_count() {
        let p = prior("", "W1AW 100\nK1ABC 10");
        let ratio = p.reading_bonus("W1AW") / p.reading_bonus("K1ABC");
        assert!((ratio - 2.0).abs() < 0.1);
    }
    #[test]
    fn allocated_prefix_earns_small_share() {
        let p = LanguagePrior::new(
            Arc::new(ReferenceLists::new(
                Arc::new(cty::Table::bundled()),
                Arc::new(scp::Set::parse("")),
            )),
            CallsHeard::default(),
            2.0,
        );
        assert_eq!(p.reading_bonus("W1AW"), 0.2);
        assert_eq!(p.reading_bonus("QQ9ZZZ"), 0.0);
    }
    #[test]
    fn common_cw_words_hit_and_misses_do_not() {
        let p = prior("", "");
        for word in ["CQ", "DE", "5NN", "599", "QRZ", "TU", "TNX"] {
            assert!(p.reading_bonus(word) > 0.0);
        }
        assert_eq!(p.reading_bonus("Q9Z"), 0.0);
        assert_eq!(p.reading_bonus("R"), 0.0);
    }
    #[test]
    fn bonus_is_capped_at_prior_weight() {
        let p = prior("W1AW", "W1AW 18446744073709551615");
        assert!(p.reading_bonus("W1AW") <= 2.0);
    }
    #[test]
    fn calls_heard_parse_counts_comments_and_bom() {
        let p = CallsHeard::parse("\u{feff}#header\n!!header\nw1aw 12\nW1AW\nK1ABC\nbad count\n");
        assert_eq!(p.0.len(), 2);
        assert_eq!(p.0["W1AW"], 13);
    }
    #[test]
    fn absent_calls_heard_file_yields_empty_map() {
        let path = std::env::temp_dir().join(format!("manta-man317-absent-{}", std::process::id()));
        assert!(CallsHeard::load(&path).unwrap().0.is_empty());
    }
    #[test]
    fn reference_refresh_changes_prior_evidence() {
        let lists = Arc::new(ReferenceLists::new(
            Arc::new(cty::Table::parse("")),
            Arc::new(scp::Set::parse("")),
        ));
        let p = LanguagePrior::new(lists.clone(), CallsHeard::default(), 2.0);
        assert_eq!(p.reading_bonus("W1AW"), 0.0);
        lists.scp.store(Arc::new(scp::Set::parse("W1AW")));
        assert_eq!(p.reading_bonus("W1AW"), 0.7);
    }
}
