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
}
