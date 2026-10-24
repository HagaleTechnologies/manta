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

/// CER of `expected` against the frequency-joined copy of `tracks`
/// (MAN-116 D6, shared by MAN-120/FU-3): the texts of every track whose
/// frequency sits within `match_hz` of `freq_hz`, joined in ascending
/// track-id order with single spaces, empty texts skipped. Fading
/// fragments one station into several tracks; scoring against only one
/// of them (nearest, lowest id, ...) would score a fragmented station as
/// lost even when its copy is fine. With no track in the window the
/// joined copy is empty and the CER is 1. The caller applies its own
/// `.min(1.0)`/ceilings -- this returns the raw ratio.
pub fn joined_cer(expected: &str, tracks: &[(u64, f64, &str)], freq_hz: f64, match_hz: f64) -> f64 {
    let mut copy: Vec<(u64, &str)> = tracks
        .iter()
        .filter(|&(_, f, text)| (f - freq_hz).abs() <= match_hz && !text.is_empty())
        .map(|&(id, _, text)| (id, text))
        .collect();
    copy.sort_by_key(|&(id, _)| id);
    let joined = copy.into_iter().map(|(_, text)| text).collect::<Vec<_>>();
    cer(expected, &joined.join(" "))
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

    // joined_cer (MAN-120, the MAN-116 D6 frequency-join convention).

    #[test]
    fn joins_two_fragments_in_id_order() {
        // The vec lists the later fragment first; the join follows the
        // ids, so the two halves read in transmission order.
        let tracks = [
            (7u64, 14_000_060.0f64, "W1AW K"),
            (3u64, 14_000_000.0f64, "CQ CQ DE"),
        ];
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_030.0, 150.0),
            0.0
        );
        // In vec order instead, the halves would read backwards.
        assert!(cer("CQ CQ DE W1AW K", "W1AW K CQ CQ DE") > 0.25);
    }

    #[test]
    fn skips_empty_text_tracks() {
        // An empty-text track within the window contributes nothing --
        // not even a stray space in the join.
        let with_empty = [
            (1u64, 14_000_000.0f64, ""),
            (2u64, 14_000_010.0, "CQ CQ DE W1AW K"),
        ];
        let without: &[(u64, f64, &str)] = &[(2u64, 14_000_010.0, "CQ CQ DE W1AW K")];
        let expected = cer("CQ CQ DE W1AW K", "CQ CQ DE W1AW K");
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &with_empty, 14_000_005.0, 150.0),
            expected
        );
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", without, 14_000_005.0, 150.0),
            expected
        );
    }

    #[test]
    fn ignores_tracks_beyond_match_hz() {
        // A nearer-but-out-of-window track must not contribute.
        let tracks = [
            (1u64, 14_000_100.0f64, "COMPLETELY WRONG"),
            (2u64, 14_000_400.0, "CQ CQ DE W1AW K"),
        ];
        // 14_000_400 is 300 Hz from 14_000_100: outside a 150 Hz window
        // around the probe, inside a 350 Hz one.
        assert!(joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_100.0, 150.0) > 0.8);
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_000_400.0, 150.0),
            0.0
        );
    }

    #[test]
    fn no_tracks_within_window_is_cer_one() {
        let tracks = [(1u64, 14_000_000.0f64, "CQ CQ DE W1AW K")];
        assert_eq!(
            joined_cer("CQ CQ DE W1AW K", &tracks, 14_001_000.0, 150.0),
            1.0
        );
        assert_eq!(joined_cer("CQ CQ DE W1AW K", &[], 14_001_000.0, 150.0), 1.0);
    }
}
