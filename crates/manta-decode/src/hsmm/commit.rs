//! Partial-traceback commit and posterior margins. SPEC v2 §4.8–4.9.
use super::segments::SegType;
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
/// decided once one was emitted at or after its character's start, or
/// when its gap began before the latest emitted entry; a boundary whose
/// character was stripped as *contradicting* the emitted glyph is
/// stripped together with that character (`strip_decided`), while one
/// whose character agreed keeps its chance to emit. Every check is O(1)
/// and needs no pruning.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Frontier {
    /// `sample_ts` of the latest emitted entry of either kind.
    last_ts: Option<u64>,
    /// `sample_ts` of the latest emitted word boundary.
    last_word_ts: Option<u64>,
    /// Glyph of the latest emitted character entry.
    last_glyph: Option<Glyph>,
    /// `char_start_ts` of the latest emitted character entry.
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

    /// Strip every decided entry from `hist`.
    ///
    /// MAN-189 validate attempt 1: a decided character entry that
    /// CONTRADICTS the latest emitted glyph (a different glyph over
    /// already-decided time, at another start than the emitted
    /// character's own) takes its trailing word boundary with it -- left
    /// alone, that boundary would emit a space and push the frontier
    /// past the real next character. A decided character that AGREES
    /// with the emitted glyph keeps its boundary, whatever anchor its
    /// first mark sat at: the WGap lineage's boundary after the CGap
    /// lineage won the character must still be emitted once (the plan's
    /// Design consequence 5). An earlier form of this rule compared
    /// `char_start_ts` values against the last emitted character's start
    /// ALONE and also stripped the boundaries of agreeing lineages,
    /// losing the space between two words. A same-start contradicting
    /// straggler keeps its boundary, exactly as before: the glyph test
    /// is what decides contradiction, not the start alone, and the start
    /// test only scopes which stragglers' boundaries may follow a
    /// stripped character into the decided prefix.
    pub(crate) fn strip_decided(&mut self, hist: &mut Vec<HistEntry>) {
        let mut i = 0;
        while i < hist.len() {
            let e = hist[i];
            if self.is_decided(&e) {
                hist.remove(i);
                if e.glyph.is_some_and(|g| Some(g) != self.last_glyph)
                    && self.last_char_start != Some(e.char_start_ts)
                    && hist.get(i).is_some_and(|w| w.glyph.is_none())
                {
                    hist.remove(i);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Record that `e` was emitted.
    pub fn advance(&mut self, e: &HistEntry) {
        self.last_ts = self.last_ts.max(Some(e.sample_ts));
        match e.glyph {
            Some(g) => {
                self.last_glyph = Some(g);
                self.last_char_start = Some(e.char_start_ts);
            }
            None => self.last_word_ts = self.last_word_ts.max(Some(e.sample_ts)),
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
/// form a prefix and the front-only strip is complete.
///
/// Internal commit result for `HsmmDecoder::push` (MAN-192): the public
/// entries plus whether this call actually emitted a winning word-boundary
/// head whose closing segment was `Silence` -- SPEC v2 §4.7's second
/// reseeding trigger. Glyph consensus alone cannot establish a committed
/// long gap (CGap, WGap and Silence interpretations can agree on the
/// glyph), so only the boundary entry's own provenance, read from the
/// head selected after the sort and duplicate stripping, sets the flag.
pub(super) struct CommitOutcome {
    pub entries: Vec<Committed>,
    pub committed_silence: bool,
}

pub fn commit(
    tokens: &mut Vec<Token>,
    now_hop: u64,
    cfg: &HsmmConfig,
    frontier: &mut Frontier,
) -> Vec<Committed> {
    commit_outcome(tokens, now_hop, cfg, frontier).entries
}

pub(super) fn commit_outcome(
    tokens: &mut Vec<Token>,
    now_hop: u64,
    cfg: &HsmmConfig,
    frontier: &mut Frontier,
) -> CommitOutcome {
    let mut out = Vec::new();
    let mut committed_silence = false;
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
        if head.glyph.is_none() && head.closing_segment == Some(SegType::Silence) {
            committed_silence = true;
        }
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
    CommitOutcome {
        entries: out,
        committed_silence,
    }
}

#[cfg(test)]
mod tests {
    use super::super::segments::SegType;
    use super::super::token::{HistEntry, Phase};
    use super::*;

    fn entry(glyph: Option<char>, char_start_ts: u64, sample_ts: u64) -> HistEntry {
        HistEntry {
            glyph: glyph.map(Glyph::Char),
            sample_ts,
            born_hop: 1,
            char_start_ts,
            closing_segment: None,
        }
    }

    /// MAN-192: compact fixture builder -- a synthetic history entry with
    /// explicit `born_hop` and closing-segment provenance (`None` = no
    /// closing segment, the honest value for a hand-built fixture).
    fn seg_entry(
        glyph: Option<Glyph>,
        sample_ts: u64,
        born_hop: u64,
        seg: Option<SegType>,
    ) -> HistEntry {
        HistEntry {
            glyph,
            sample_ts,
            born_hop,
            char_start_ts: 0,
            closing_segment: seg,
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
    fn commit_keeps_an_agreeing_lineages_word_boundary_at_another_start() {
        // MAN-189 validate attempt 1: two lineages agree on G at the same
        // close but start it at different anchors. The winner's G commits
        // and the runner-up's agreeing G is stripped as decided -- its
        // word boundary must SURVIVE (Design consequence 5: the WGap
        // lineage's boundary after the CGap lineage won). The earlier
        // start-comparison form of rule 9 stripped it and lost the space.
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
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(Some(Glyph::Char('G')), 100)]);
        assert_eq!(
            tokens[1].hist,
            vec![entry(None, 48, 100)],
            "the agreeing lineage's word boundary must survive the commit"
        );
        // Once the CGap lineage dies, the retained boundary emits once.
        let mut tokens = vec![token(4.0, 13.0, vec![entry(None, 48, 100)])];
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(emitted(&out), vec![(None, 100)]);
    }

    #[test]
    fn commit_keeps_a_later_first_mark_variants_word_boundary() {
        // MAN-189 validate attempt 1, scratch 2: the same G re-read with
        // its first mark placed at a later anchor (Design consequence 2's
        // variant) is stripped as decided, but its word boundary and the
        // following character still emit -- the space must not be lost.
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
        let out = commit(&mut tokens, FORCE, &cfg, &mut frontier);
        assert_eq!(
            emitted(&out),
            vec![(None, 104), (Some(Glyph::Char('H')), 200)]
        );
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

    /// MAN-192: `commit_outcome` sets `committed_silence` only when it
    /// actually emits a winning word-boundary head whose own provenance is
    /// `Silence`. A WGap boundary, a CGap glyph (no boundary at all), and
    /// a boundary with unknown (`None`) provenance must not set it.
    #[test]
    fn man192_commit_outcome_flags_only_a_silence_boundary_head() {
        let cfg = HsmmConfig::default();
        let cases: [(Option<SegType>, Option<SegType>, bool); 3] = [
            // (glyph-entry provenance, boundary-entry provenance, signal)
            (Some(SegType::Silence), Some(SegType::Silence), true),
            (Some(SegType::WGap), Some(SegType::WGap), false),
            (Some(SegType::CGap), None, false), // CGap: glyph only, no boundary
        ];
        for (i, (glyph_seg, boundary_seg, expect)) in cases.iter().enumerate() {
            let mut hist = vec![seg_entry(Some(Glyph::Char('T')), 512, 100, *glyph_seg)];
            if let Some(bs) = boundary_seg {
                hist.push(seg_entry(None, 512, 100, Some(*bs)));
            }
            let hist_len = hist.len();
            let mut tokens = vec![token(10.0, 13.0, hist)];
            let mut frontier = Frontier::default();
            let out = commit_outcome(&mut tokens, 100, &cfg, &mut frontier);
            assert_eq!(
                out.entries.len(),
                hist_len,
                "case {i}: consensus of one token must emit every entry"
            );
            assert_eq!(
                out.committed_silence, *expect,
                "case {i}: signal must follow the boundary head's provenance"
            );
        }
        // Unknown (`None`) boundary provenance -- a synthetic entry no real
        // gap transition produced -- must not trigger reseeding either.
        let mut tokens = vec![token(
            10.0,
            13.0,
            vec![
                seg_entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::CGap)),
                seg_entry(None, 512, 100, None),
            ],
        )];
        let mut frontier = Frontier::default();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut frontier);
        assert_eq!(out.entries.len(), 2);
        assert!(
            !out.committed_silence,
            "a None-provenance boundary must not flag"
        );
    }

    /// MAN-192: the signal reads the WINNING head's provenance (the token
    /// that sorts first), not any agreeing or losing candidate. Two tokens
    /// carrying the same glyphs closed by different segment types agree on
    /// everything the consensus check sees, so only the sort order -- and
    /// with disagreeing tokens, the forced path -- decides.
    #[test]
    fn man192_winning_head_provenance_decides_the_signal() {
        let cfg = HsmmConfig::default();
        let hist = |seg: SegType| {
            vec![
                seg_entry(Some(Glyph::Char('T')), 512, 100, Some(seg)),
                seg_entry(None, 512, 100, Some(seg)),
            ]
        };
        // Consensus path: both tokens agree on the glyphs; the higher
        // score sorts first and its provenance wins. A losing Silence
        // hypothesis must NOT cause the signal.
        let mut tokens = vec![
            token(10.0, 13.0, hist(SegType::WGap)),
            token(9.0, 13.0, hist(SegType::Silence)),
        ];
        let mut frontier = Frontier::default();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut frontier);
        assert_eq!(
            out.entries.len(),
            2,
            "consensus commits glyph then boundary"
        );
        assert!(!out.committed_silence, "the winning head closed via WGap");

        let mut tokens = vec![
            token(9.0, 13.0, hist(SegType::WGap)),
            token(10.0, 13.0, hist(SegType::Silence)),
        ];
        let mut frontier = Frontier::default();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut frontier);
        assert!(
            out.committed_silence,
            "the winning head closed via Silence -- even though the loser closed via WGap"
        );

        // Forced path: tokens disagree on the glyph; only the far-past
        // born_hop forces emission, and the winner's provenance still
        // decides.
        let disagree: Vec<HistEntry> = vec![seg_entry(
            Some(Glyph::Char('E')),
            300,
            1,
            Some(SegType::WGap),
        )];
        let mut tokens = vec![
            token(10.0, 13.0, hist(SegType::Silence)),
            token(9.5, 13.0, disagree),
        ];
        let mut frontier = Frontier::default();
        let out = commit_outcome(&mut tokens, 100_000, &cfg, &mut frontier);
        assert_eq!(
            out.entries.len(),
            2,
            "forced commit emits the winner's entries"
        );
        assert!(out.committed_silence, "forced winner closed via Silence");
    }

    /// MAN-192: a glyph and its boundary can commit in separate calls --
    /// only the call that actually emits the Silence boundary reports it.
    #[test]
    fn man192_split_commit_reports_silence_only_on_the_boundary_call() {
        let cfg = HsmmConfig::default();
        // Best token: T closed by Silence (glyph + boundary). Runner-up:
        // T closed by CGap (glyph only) -- it agrees on the glyph but has
        // no boundary entry, so the boundary cannot reach consensus.
        let silence_hist = vec![
            seg_entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
            seg_entry(None, 512, 100, Some(SegType::Silence)),
        ];
        let cgap_hist = vec![seg_entry(
            Some(Glyph::Char('T')),
            512,
            100,
            Some(SegType::CGap),
        )];
        let mut tokens = vec![token(10.0, 13.0, silence_hist), token(9.0, 13.0, cgap_hist)];
        let mut frontier = Frontier::default();
        // First call, not forced: the glyph reaches consensus, the boundary
        // does not (the CGap runner-up disagrees by having no entry).
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut frontier);
        assert_eq!(
            out.entries.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('T'))],
            "only the glyph commits on the first call"
        );
        assert!(!out.committed_silence, "no boundary was emitted yet");
        // Second call: the runner-up's glyph entry is now decided
        // (character start at or before the frontier) and stripped; force
        // the still-pending boundary with a far-future now_hop.
        let out = commit_outcome(&mut tokens, 100_000, &cfg, &mut frontier);
        assert_eq!(
            out.entries.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![None],
            "the boundary commits on the second call"
        );
        assert!(
            out.committed_silence,
            "the boundary head closed via Silence"
        );
        // Third call: re-deriving the already-decided boundary yields no
        // entries and no signal.
        let mut stale = vec![token(
            10.0,
            13.0,
            vec![
                seg_entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
                seg_entry(None, 512, 100, Some(SegType::Silence)),
            ],
        )];
        let out = commit_outcome(&mut stale, 200_000, &cfg, &mut frontier);
        assert!(
            out.entries.is_empty(),
            "an already-decided boundary re-emits nothing"
        );
        assert!(!out.committed_silence, "and reports no signal");
    }

    /// MAN-192 Phase 1 success criterion: the public `commit` wrapper and
    /// the internal `commit_outcome` helper produce identical entries
    /// (including confidence and alternatives) on cloned inputs.
    #[test]
    fn man192_public_wrapper_matches_internal_helper_entries() {
        let cfg = HsmmConfig::default();
        let make = || {
            vec![
                token(
                    10.0,
                    13.0,
                    vec![
                        seg_entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
                        seg_entry(None, 512, 100, Some(SegType::Silence)),
                    ],
                ),
                token(
                    7.0,
                    13.0,
                    vec![
                        seg_entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::WGap)),
                        seg_entry(None, 512, 100, Some(SegType::WGap)),
                    ],
                ),
            ]
        };
        let mut tokens_a = make();
        let mut frontier_a = Frontier::default();
        let via_wrapper = commit(&mut tokens_a, 100, &cfg, &mut frontier_a);
        let mut tokens_b = make();
        let mut frontier_b = Frontier::default();
        let via_helper = commit_outcome(&mut tokens_b, 100, &cfg, &mut frontier_b);
        assert_eq!(via_wrapper.len(), via_helper.entries.len());
        for (w, h) in via_wrapper.iter().zip(via_helper.entries.iter()) {
            assert_eq!(w.glyph, h.glyph);
            assert_eq!(w.sample_ts, h.sample_ts);
            assert_eq!(w.confidence.to_bits(), h.confidence.to_bits());
            assert_eq!(w.alternatives.len(), h.alternatives.len());
            for (wa, ha) in w.alternatives.iter().zip(h.alternatives.iter()) {
                assert_eq!(wa.0, ha.0);
                assert_eq!(wa.1.to_bits(), ha.1.to_bits());
            }
        }
        assert_eq!(frontier_a, frontier_b, "both paths advance identically");
    }
}
