//! Character error rate: Levenshtein distance / expected length.

fn normalize(s: &str) -> Vec<char> {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
        .chars()
        .collect()
}

/// Character error rate: Levenshtein distance over expected length,
/// case/whitespace-normalized. Per ROADMAP.md's CER convention.
pub fn cer(expected: &str, decoded: &str) -> f64 {
    let e = normalize(expected);
    let d = normalize(decoded);
    if e.is_empty() {
        return if d.is_empty() { 0.0 } else { 1.0 };
    }
    // Two-row Levenshtein DP.
    let mut prev: Vec<usize> = (0..=d.len()).collect();
    let mut cur = vec![0usize; d.len() + 1];
    for i in 1..=e.len() {
        cur[0] = i;
        for j in 1..=d.len() {
            let sub = prev[j - 1] + usize::from(e[i - 1] != d[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[d.len()] as f64 / e.len() as f64
}

/// 1 - CER. Per ROADMAP.md's CER convention.
pub fn char_accuracy(expected: &str, decoded: &str) -> f64 {
    1.0 - cer(expected, decoded)
}

/// One aligned position between expected and decoded text (MAN-8 Phase 1:
/// error-localization support for the QSB-phase diagnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOp {
    Match {
        expected_idx: usize,
        decoded_idx: usize,
    },
    Substitute {
        expected_idx: usize,
        decoded_idx: usize,
    },
    /// Expected char missing from the decode.
    Delete { expected_idx: usize },
    /// Spurious char in the decode.
    Insert { decoded_idx: usize },
}

/// Levenshtein alignment (same case/whitespace normalization as `cer`), for
/// error localization: `(ops non-Match count) / normalize(expected).len()`
/// equals `cer(expected, decoded)` exactly, since both walk the same edit-
/// distance recurrence. Test-only tool (not on any decode hot path): builds
/// the full DP matrix rather than `cer`'s two-row rolling form, so the
/// backtrace has something to walk.
///
/// Tie-break is deterministic: at each cell, a match/substitute step is
/// preferred over a delete over an insert, whenever more than one achieves
/// the optimal cost.
pub fn align(expected: &str, decoded: &str) -> Vec<EditOp> {
    let e = normalize(expected);
    let d = normalize(decoded);
    let (m, n) = (e.len(), d.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let sub_cost = usize::from(e[i - 1] != d[j - 1]);
            dp[i][j] = (dp[i - 1][j - 1] + sub_cost)
                .min(dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1);
        }
    }
    let mut ops = Vec::with_capacity(m.max(n));
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 {
            let sub_cost = usize::from(e[i - 1] != d[j - 1]);
            if dp[i][j] == dp[i - 1][j - 1] + sub_cost {
                ops.push(if sub_cost == 0 {
                    EditOp::Match {
                        expected_idx: i - 1,
                        decoded_idx: j - 1,
                    }
                } else {
                    EditOp::Substitute {
                        expected_idx: i - 1,
                        decoded_idx: j - 1,
                    }
                });
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && dp[i][j] == dp[i - 1][j] + 1 {
            ops.push(EditOp::Delete { expected_idx: i - 1 });
            i -= 1;
            continue;
        }
        debug_assert!(j > 0 && dp[i][j] == dp[i][j - 1] + 1);
        ops.push(EditOp::Insert { decoded_idx: j - 1 });
        j -= 1;
    }
    ops.reverse();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_zero() {
        assert_eq!(cer("CQ CQ DE W1AW", "CQ CQ DE W1AW"), 0.0);
        assert_eq!(cer("CQ  CQ", "cq cq"), 0.0); // normalization
    }

    #[test]
    fn substitution_counts() {
        // 1 edit over 5 chars
        assert!((cer("PARIS", "PARIX") - 0.2).abs() < 1e-9);
    }

    #[test]
    fn insertion_and_deletion_count() {
        assert!((cer("PARIS", "PARIS5") - 0.2).abs() < 1e-9);
        assert!((cer("PARIS", "PAIS") - 0.2).abs() < 1e-9);
        assert!((char_accuracy("PARIS", "PARIS") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn align_reports_edit_ops_positionally() {
        // "CQ DE K" vs "CQ E K": one deletion at expected index 3 ('D').
        let ops = align("CQ DE K", "CQ E K");
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, EditOp::Delete { .. }))
                .count(),
            1
        );
        assert!(matches!(
            ops[3],
            EditOp::Delete { expected_idx: 3 }
        ));
        // Sum of non-Match ops must equal the raw Levenshtein distance.
        let dist = ops.iter().filter(|o| !matches!(o, EditOp::Match { .. })).count();
        assert_eq!(dist as f64 / 7.0, cer("CQ DE K", "CQ E K"));
    }

    #[test]
    fn align_is_consistent_with_cer_on_the_v6_sample() {
        let e = "CQ CQ DE K5ZZZ K5ZZZ K";
        let d = "CQ DE K5ZZZ K5ZZZ CQ CQ E K5ZZZ";
        let ops = align(e, d);
        let dist = ops.iter().filter(|o| !matches!(o, EditOp::Match { .. })).count();
        assert!((dist as f64 / normalize(e).len() as f64 - cer(e, d)).abs() < 1e-12);
    }
}
