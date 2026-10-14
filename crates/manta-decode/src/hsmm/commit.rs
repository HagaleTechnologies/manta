//! Partial-traceback commit and posterior margins. SPEC v2 §4.8–4.9.
use super::token::{glyph_rank, token_order, HistEntry, Token};
use super::{Committed, HsmmConfig};
use crate::tree::Glyph;

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Confidence and alternatives for entry `j` of the best token (tokens
/// must be sorted by `token_order`, best first).
pub fn margin(tokens: &[Token], j: usize, cfg: &HsmmConfig) -> (f32, Vec<(Glyph, f32)>) {
    let best = &tokens[0];
    let g = best.hist.get(j).map(|e| e.glyph);
    let mut alts: Vec<(Option<Glyph>, f32)> = Vec::new();
    // Codex review, PR #161 round 2: `s_alt` (the runner-up score the
    // margin is computed against) must include every token that
    // DISAGREES with `best` at entry `j` -- including one that hasn't
    // appended entry `j` at all yet (`tg` outer `None`, e.g. it's shorter
    // or behind on a forced commit). The old code only ever folded
    // `alts`' own scores into `s_alt`, and `alts` only ever holds tokens
    // with an actual glyph/boundary value to report as an alternative --
    // an undecided token has none, so it was silently excluded from
    // `s_alt` too, letting a real, still-live, near-tied competitor
    // inflate confidence toward sigmoid(4kappa/kappa) ~= 0.982 instead of
    // the ~0.5 its true score gap implies. Track `s_alt` independently of
    // `alts` so every disagreeing token counts toward the margin, while
    // `alts` (the reported alternative list) still only ever contains
    // tokens with a real glyph/boundary to show.
    let mut s_alt = f32::NEG_INFINITY;
    for t in tokens.iter().skip(1) {
        let tg = t.hist.get(j).map(|e| e.glyph);
        if tg != g {
            s_alt = s_alt.max(t.score);
            if let Some(og) = tg.flatten() {
                if !alts.iter().any(|(a, _)| *a == Some(og)) {
                    alts.push((Some(og), t.score));
                }
            } else if tg.is_some() && !alts.iter().any(|(a, _)| a.is_none()) {
                alts.push((None, t.score));
            }
        }
    }
    let s_alt = if s_alt.is_finite() {
        s_alt
    } else {
        best.score - 4.0 * cfg.conf_kappa
    };
    let confidence = sigmoid((best.score - s_alt) / cfg.conf_kappa);
    alts.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| glyph_rank(a.0).cmp(&glyph_rank(b.0)))
    });
    let alternatives = alts
        .into_iter()
        .filter_map(|(g, s)| g.map(|g| (g, sigmoid((s - best.score) / cfg.conf_kappa))))
        .take(2)
        .collect();
    (confidence, alternatives)
}

/// MAN-189: what this decoder has already emitted, kept as a time
/// frontier rather than a list of exact timestamps.
///
/// Emitted entries form one time-ordered segmentation of the signal. A
/// character whose first mark began at or before the latest emitted
/// entry's `sample_ts` lies in time that is already decided: it is the
/// same character again under a different closing anchor, or a
/// segmentation that contradicts what was emitted. A word boundary is
/// decided once one was emitted at or after its character's start, when
/// its gap began before the latest emitted entry, or when it follows a
/// stripped character with both a different glyph and first-mark anchor
/// from the last emitted one.
/// Different first-mark anchors alone do not imply glyph disagreement.
/// Every entry check is O(1) and needs no pruning.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Frontier {
    /// `sample_ts` of the latest emitted entry of either kind.
    last_ts: Option<u64>,
    /// `sample_ts` of the latest emitted word boundary.
    last_word_ts: Option<u64>,
    /// Glyph of the latest emitted character, for rejecting its alternatives.
    last_char: Option<Glyph>,
    /// First-mark anchor of that character. Same-start alternatives retain
    /// the plan's original boundary handling.
    last_char_start: Option<u64>,
}

impl Frontier {
    /// Whether `e` covers time already emitted, so must not be emitted.
    pub fn is_decided(&self, e: &HistEntry) -> bool {
        match e.glyph {
            Some(_) => self.last_ts.is_some_and(|t| e.char_start_ts <= t),
            None => {
                self.last_word_ts.is_some_and(|w| e.char_start_ts <= w)
                    || self.last_ts.is_some_and(|t| e.sample_ts < t)
            }
        }
    }

    /// Strip decided entries, including the boundary paired with a rejected
    /// character with a different glyph and start. Both entries of a WGap
    /// share their start and close; an agreeing character may use different
    /// anchors from the emitted one and must still be allowed its boundary.
    pub fn strip_decided(&self, hist: &mut Vec<HistEntry>) {
        let mut rejected_boundary = None;
        hist.retain(|e| {
            let decided = self.is_decided(e);
            let anchors = (e.char_start_ts, e.sample_ts);
            if e.glyph.is_some() {
                rejected_boundary = (decided
                    && e.glyph != self.last_char
                    && Some(e.char_start_ts) != self.last_char_start)
                    .then_some(anchors);
                !decided
            } else {
                let rejected = rejected_boundary.take() == Some(anchors);
                !decided && !rejected
            }
        });
    }

    /// Record that `e` was emitted.
    pub fn advance(&mut self, e: &HistEntry) {
        self.last_ts = self.last_ts.max(Some(e.sample_ts));
        if e.glyph.is_none() {
            self.last_word_ts = self.last_word_ts.max(Some(e.sample_ts));
        } else {
            self.last_char = e.glyph;
            self.last_char_start = Some(e.char_start_ts);
        }
    }
}

/// Emit the consensus prefix and any entry older than the lookahead;
/// remove emitted entries from every token; drop tokens that disagree
/// with a forced commit. `tokens` is sorted best-first on entry and
/// stays sorted.
///
/// `HsmmDecoder::push` keeps past anchors alive (up to `reach_sil` hops
/// back, SPEC v2 §4.6), so a later hop can re-derive a gap this decoder
/// already committed: from the same anchor with a larger `d` (PR #161,
/// Task 8), with a different glyph (round 6), or, MAN-189, with an
/// agreeing glyph closed at a different anchor and so a different
/// `sample_ts`. Every such entry's character began at or before
/// `frontier`, so it is stripped at the top of every loop iteration,
/// before the consensus/forced check sees it. Character starts strictly
/// increase along a token's `hist` (a word boundary shares its
/// character's start and closing `sample_ts`), so decided entries always
/// form a prefix. The history filter also removes a boundary paired with
/// a stripped character with a different glyph and first-mark anchor.
pub fn commit(
    tokens: &mut Vec<Token>,
    now_hop: u64,
    cfg: &HsmmConfig,
    frontier: &mut Frontier,
) -> Vec<Committed> {
    let mut out = Vec::new();
    loop {
        for t in tokens.iter_mut() {
            frontier.strip_decided(&mut t.hist);
        }
        // Codex review, PR #161 round 15: stripping decided entries
        // removes a DIFFERENT number of leading entries per token
        // (unlike the uniform consensus-glyph removal near the end of
        // this loop, which strips the same shared leading entry from
        // every survivor and so can never change their relative lexical
        // order) -- two equal-score tokens whose decided leading glyphs
        // differ can have their SPEC v2 §6.2 tie-break order silently
        // invalidated once stripped, e.g. `[A(decided), Z]` and
        // `[B(decided), A]` are ordered first-then-second before
        // stripping but should reverse after (`[Z]` vs `[A]`). Re-sort
        // so `tokens[0]` below, the forced-commit check, and `margin`
        // all see the correct deterministic best token.
        tokens.sort_by(token_order);
        if tokens.is_empty() || tokens[0].hist.is_empty() {
            break;
        }
        let head = tokens[0].hist[0];
        let consensus = tokens
            .iter()
            .all(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        // `saturating_sub`: `now_hop` is always >= any live token's
        // `born_hop` (both derived from the same monotonic evidence hop
        // counter), but make that invariant explicit rather than relying
        // on it silently.
        let forced =
            now_hop.saturating_sub(head.born_hop) as f32 > cfg.lookahead_dits * tokens[0].u;
        if !consensus && !forced {
            break;
        }
        let (confidence, alternatives) = margin(tokens, 0, cfg);
        out.push(Committed {
            glyph: head.glyph,
            sample_ts: head.sample_ts,
            confidence,
            alternatives,
        });
        tokens.retain(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        for t in tokens.iter_mut() {
            t.hist.remove(0);
        }
        frontier.advance(&head);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::token::Phase;
    use super::*;

    fn entry(glyph: Option<char>, char_start_ts: u64, sample_ts: u64) -> HistEntry {
        HistEntry {
            glyph: glyph.map(Glyph::Char),
            sample_ts,
            born_hop: 1,
            char_start_ts,
        }
    }

    fn token(score: f32, u: f32, hist: Vec<HistEntry>) -> Token {
        Token {
            node: 0,
            phase: Phase::AfterSpace,
            u,
            score,
            hist,
            anchor_hop: 2,
            char_start_ts: 0,
        }
    }

    /// The frontier after emitting `entries` in order.
    fn after(entries: &[HistEntry]) -> Frontier {
        let mut f = Frontier::default();
        for e in entries {
            f.advance(e);
        }
        f
    }

    fn emitted(out: &[Committed]) -> Vec<(Option<Glyph>, u64)> {
        out.iter().map(|c| (c.glyph, c.sample_ts)).collect()
    }

    // Far enough past every entry's `born_hop` to force a commit.
    const FORCE: u64 = 1000;

    #[test]
    fn commit_does_not_reemit_a_glyph_under_another_agreeing_timestamp() {
        // MAN-189's scenario: two lineages agree on Q and its start but
        // close it at different anchors. The best token's Q@100 commits;
        // the runner-up's agreeing Q@130 must not surface later.
        let cfg = HsmmConfig::default();
        let mut frontier = Frontier::default();
        let mut tokens = vec![
            token(5.0, 13.0, vec![entry(Some('Q'), 40, 100)]),
            token(4.0, 13.0, vec![entry(Some('Q'), 40, 130)]),
        ];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(Some(Glyph::Char('Q')), 100)]);

        let mut tokens = vec![token(4.0, 13.0, vec![entry(Some('Q'), 40, 130)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert!(out.is_empty(), "re-emitted the committed Q: {out:?}");
        assert!(tokens[0].hist.is_empty(), "{:?}", tokens[0].hist);
    }

    #[test]
    fn commit_does_not_reemit_a_glyph_whose_first_mark_was_placed_later() {
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('Q'), 40, 100)]);
        let mut tokens = vec![token(5.0, 13.0, vec![entry(Some('Q'), 52, 104)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn commit_still_emits_the_same_glyph_again_when_it_starts_after_the_commit() {
        // Sealing by glyph value would be wrong: VR3's N5ZZ holds two Zs.
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('Z'), 40, 100)]);
        let mut tokens = vec![token(5.0, 13.0, vec![entry(Some('Z'), 140, 200)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(Some(Glyph::Char('Z')), 200)]);
    }

    #[test]
    fn commit_never_emits_a_character_that_overlaps_emitted_time() {
        // A conflicting segmentation that closes before the committed B
        // would be emitted out of order ("T after B").
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('B'), 150, 200)]);
        let mut tokens = vec![token(
            5.0,
            13.0,
            vec![entry(Some('T'), 120, 180), entry(Some('E'), 240, 260)],
        )];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(Some(Glyph::Char('E')), 260)]);
    }

    #[test]
    fn commit_emits_a_word_boundary_after_its_already_emitted_character() {
        // The WGap lineage can win the boundary after the CGap lineage
        // won the character at the same gap; the boundary still counts.
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('G'), 40, 100)]);
        let mut tokens = vec![token(5.0, 13.0, vec![entry(None, 40, 100)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(None, 100)]);
    }

    #[test]
    fn commit_keeps_the_word_boundary_of_an_agreeing_lineage_with_another_start() {
        let cfg = HsmmConfig::default();
        let mut frontier = Frontier::default();
        let mut tokens = vec![
            token(5.0, 13.0, vec![entry(Some('G'), 40, 100)]),
            token(
                4.0,
                13.0,
                vec![entry(Some('G'), 48, 100), entry(None, 48, 100)],
            ),
        ];
        assert_eq!(
            emitted(&commit(&mut tokens, 2, &cfg, &mut frontier)),
            vec![(Some(Glyph::Char('G')), 100)]
        );
        // The CGap lineage dies, leaving the agreeing WGap lineage.
        tokens.remove(0);
        assert_eq!(
            emitted(&commit(&mut tokens, FORCE, &cfg, &mut frontier)),
            vec![(None, 100)]
        );
    }

    #[test]
    fn commit_keeps_the_word_boundary_of_a_later_first_mark_variant() {
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('G'), 40, 100)]);
        let mut tokens = vec![token(
            5.0,
            13.0,
            vec![
                entry(Some('G'), 52, 104),
                entry(None, 52, 104),
                entry(Some('H'), 150, 200),
            ],
        )];
        assert_eq!(
            emitted(&commit(&mut tokens, FORCE, &cfg, &mut frontier)),
            vec![(None, 104), (Some(Glyph::Char('H')), 200)]
        );
    }

    #[test]
    fn commit_does_not_emit_a_second_word_boundary_for_the_same_character() {
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('G'), 40, 100), entry(None, 40, 100)]);
        let mut tokens = vec![token(
            5.0,
            13.0,
            vec![entry(Some('G'), 40, 130), entry(None, 40, 130)],
        )];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn commit_does_not_emit_a_word_boundary_behind_a_later_character() {
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('G'), 40, 100), entry(Some('H'), 150, 200)]);
        let mut tokens = vec![token(5.0, 13.0, vec![entry(None, 40, 100)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn commit_does_not_emit_a_word_boundary_after_a_contradicting_character() {
        // A straggler reads Y over the emitted X's time. Y is stripped;
        // its trailing boundary must go too, or it would emit a space
        // and push the frontier past the real next character.
        let cfg = HsmmConfig::default();
        let mut frontier = after(&[entry(Some('X'), 40, 100)]);
        let mut tokens = vec![token(
            5.0,
            13.0,
            vec![
                entry(Some('Y'), 60, 130),
                entry(None, 60, 130),
                entry(Some('E'), 160, 180),
            ],
        )];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(Some(Glyph::Char('E')), 180)]);
    }

    #[test]
    fn commit_output_never_moves_backwards_in_time() {
        let cfg = HsmmConfig::default();
        let mut frontier = Frontier::default();
        let calls = [
            vec![entry(Some('C'), 40, 100), entry(Some('Q'), 140, 200)],
            // A straggler re-reads Q closed one anchor early, then T.
            vec![entry(Some('Q'), 140, 190), entry(Some('T'), 240, 300)],
            vec![
                entry(Some('T'), 240, 310),
                entry(None, 240, 310),
                entry(Some('E'), 350, 400),
            ],
        ];
        let mut all = Vec::new();
        for hist in calls {
            let mut tokens = vec![token(5.0, 13.0, hist)];
            all.extend(emitted(&commit(&mut tokens, FORCE, &cfg, &mut frontier)));
        }
        let ts: Vec<u64> = all.iter().map(|&(_, ts)| ts).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "{all:?}");
        assert_eq!(ts.last(), Some(&400), "{all:?}");
    }

    #[test]
    fn commit_resorts_after_stripping_decided_prefixes_of_differing_length() {
        // Codex review, PR #161 round 15: stripping decided entries
        // removes a DIFFERENT number of leading entries per token, which
        // can invalidate the SPEC v2 §6.2 deterministic tie-break order
        // among equal-score tokens. Before stripping, [A(decided), Z]
        // correctly sorts ahead of [B(decided), A] (glyph_rank('A') <
        // glyph_rank('B')). After both leading (decided) entries are
        // stripped, the survivors are [Z] vs [A] -- the order must REVERSE
        // (glyph_rank('A') < glyph_rank('Z')), which only happens if
        // commit() re-sorts.
        let cfg = HsmmConfig::default();
        let token1 = token(
            5.0,
            13.0,
            vec![entry(Some('A'), 40, 100), entry(Some('Z'), 150, 200)],
        );
        let token2 = token(
            5.0, // tied with token1
            13.0,
            vec![entry(Some('B'), 40, 100), entry(Some('A'), 150, 250)],
        );
        // Pre-strip order is already correct per token_order (token1's
        // [A, Z] sorts ahead of token2's [B, A]) -- exactly what a real
        // caller's `cands.sort_by(token_order)` would have produced.
        let mut tokens = vec![token1, token2];
        // Both leading entries cover time already emitted.
        let mut frontier = after(&[entry(Some('X'), 40, 100)]);
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(
            out.first().map(|c| (c.glyph, c.sample_ts)),
            Some((Some(Glyph::Char('A')), 250)),
            "after stripping, token2's [A] must sort ahead of token1's [Z] -- got {out:?}"
        );
    }
}
