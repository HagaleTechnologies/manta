//! Partial-traceback commit and posterior margins. SPEC v2 §4.8–4.9.
use super::segments::SegType;
use super::token::{glyph_rank, token_order, Token};
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

/// Emit the consensus prefix and any entry older than the lookahead;
/// remove emitted entries from every token; drop tokens that disagree
/// with a forced commit. `tokens` is sorted best-first on entry and
/// stays sorted.
///
/// [Task 8 fix, execution 2026-09-09]: `sealed` is a list, owned by
/// `HsmmDecoder` and threaded through every call, of `(sample_ts, glyph)`
/// pairs this decoder has ever actually committed. `HsmmDecoder::push`
/// keeps a handful of past `Anchor`s alive (up to `reach_sil` hops back,
/// SPEC v2 §4.6) so a later hop can still generate a candidate token by
/// transitioning directly from an *older* anchor's frozen token snapshot.
/// For a still-`AfterMark` ancestor sitting in one of those old anchors,
/// SPEC v2 §4.3's flat Silence duration prior stays satisfiable across a
/// *wide*, still-growing window ([10u, 80u] hops) -- so at every
/// subsequent anchor step, `HsmmDecoder::push` re-derives a *fresh*
/// candidate closing the SAME gap with a *larger* `d`, and
/// `Token::successor` stamps that fresh `HistEntry` with `born_hop =
/// end_hop` (the current hop, correct per its own doc comment -- this is
/// a new, better-evidenced re-estimate of "how long the gap has run so
/// far"). Because `born_hop` legitimately keeps advancing on every
/// re-derivation, a watermark keyed on it (the fix's first cut) cannot
/// tell "the same decision, re-estimated" apart from "a genuinely new
/// decision", and the `forced` check below re-fires on literally every
/// subsequent anchor step for as long as the gap keeps growing --
/// reproduced directly: an isolated "C" followed by a long trailing
/// silence committed the same glyph, at the same `sample_ts`, six times
/// over (three during the run, three more at `finish`), and the full
/// "CQ TEST W5AU W5AU TEST" scene came out as
/// "CCCQQQQ TESTT T WW5AUU WW5AUU TEST".
///
/// `HistEntry::sample_ts`, by contrast, is pinned to the *anchor's own*
/// `sample_ts` (`Token::successor`'s `seg_start_ts` parameter) -- fixed
/// once when that ancestor anchor was created, identical across every
/// re-derivation from it no matter how large `d` grows. Sealing on
/// `(sample_ts, is_char)` instead correctly recognizes every re-derived
/// candidate as the same already-decided instant and strips it before
/// the consensus/forced check ever sees it, while still letting a
/// legitimately different word-boundary entry at that same instant
/// (same `sample_ts`, `glyph: None` vs `Some(_)`, since a closing
/// transition emits at most one of each) commit in its own right once
/// evidence resolves it.
///
/// [Task 8 fix, review round 6, Codex on PR #161]: the seal key is
/// `(sample_ts, is_char)` -- whether the entry is a character at all,
/// NOT the specific glyph -- because a retained older anchor re-deriving
/// an already-committed gap under a different node/speed hypothesis can
/// disagree with the winning hypothesis on WHICH character it is while
/// agreeing on WHEN and THAT it's a character. Sealing on the exact
/// `(sample_ts, glyph)` pair (the original design) only strips a
/// re-derivation that happens to name the same glyph already committed;
/// once the winning hypothesis's token falls outside its valid
/// silence-duration window and stops being re-derived, a runner-up
/// lineage's disagreeing glyph at that same `sample_ts` was never in
/// `sealed` at all and could surface as a second, contradictory
/// `CharDecoded` for an instant already resolved -- the validator then
/// appends both characters into corrupted spot text. At most one
/// character and one word-boundary decision can ever be correct for a
/// given `sample_ts` (a closing transition emits at most one of each),
/// so sealing by kind rather than by value is exactly as permissive as
/// the real invariant requires.
///
/// [Task 8 fix, review round 2]: the strip runs at the *top of every loop
/// iteration*, not just once on entry. Committing one entry can reveal a
/// new front entry on some token that a *different* token/anchor lineage
/// already sealed independently in an earlier call (e.g. one surviving
/// token holds `[X@ts1, Y@ts2, ...]` while the beam already committed a
/// different glyph `X'` at `ts1` from a sibling token, sealing `(ts1,
/// X')` -- this token's own, disagreeing `(ts1, X)` front entry must be
/// silently dropped, not re-committed as a duplicate/contradictory event,
/// once it becomes the front after some earlier removal in this same
/// call). Re-checking every iteration catches that without needing a
/// second, separately-maintained check.
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
    sealed: &mut Vec<(u64, bool)>,
) -> Vec<Committed> {
    commit_outcome(tokens, now_hop, cfg, sealed).entries
}

pub(super) fn commit_outcome(
    tokens: &mut Vec<Token>,
    now_hop: u64,
    cfg: &HsmmConfig,
    sealed: &mut Vec<(u64, bool)>,
) -> CommitOutcome {
    let mut out = Vec::new();
    let mut committed_silence = false;
    loop {
        for t in tokens.iter_mut() {
            while t
                .hist
                .first()
                .is_some_and(|e| sealed.contains(&(e.sample_ts, e.glyph.is_some())))
            {
                t.hist.remove(0);
            }
        }
        // Codex review, PR #161 round 15: stripping sealed entries
        // removes a DIFFERENT number of leading entries per token
        // (unlike the uniform consensus-glyph removal near the end of
        // this loop, which strips the same shared leading entry from
        // every survivor and so can never change their relative lexical
        // order) -- two equal-score tokens whose sealed leading glyphs
        // differ can have their SPEC v2 §6.2 tie-break order silently
        // invalidated once stripped, e.g. `[A(sealed), Z]` and
        // `[B(sealed), A]` are ordered first-then-second before
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
        sealed.push((head.sample_ts, head.glyph.is_some()));
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

    /// MAN-192: compact fixture builder -- a synthetic history entry with
    /// explicit closing-segment provenance (`None` = no closing segment,
    /// the honest value for a hand-built fixture).
    fn entry(glyph: Option<Glyph>, sample_ts: u64, born_hop: u64, seg: Option<SegType>) -> HistEntry {
        HistEntry {
            glyph,
            sample_ts,
            born_hop,
            closing_segment: seg,
        }
    }

    fn token(hist: Vec<HistEntry>, score: f32) -> Token {
        Token {
            node: 0,
            phase: Phase::AfterSpace,
            u: 13.0,
            score,
            hist,
            anchor_hop: 2,
        }
    }

    #[test]
    fn commit_resorts_after_stripping_sealed_prefixes_of_differing_length() {
        // Codex review, PR #161 round 15: stripping sealed entries removes
        // a DIFFERENT number of leading entries per token, which can
        // invalidate the SPEC v2 §6.2 deterministic tie-break order among
        // equal-score tokens. Before stripping, [A(sealed), Z] correctly
        // sorts ahead of [B(sealed), A] (glyph_rank('A') < glyph_rank('B')).
        // After both leading (sealed) entries are stripped, the survivors
        // are [Z] vs [A] -- the order must REVERSE (glyph_rank('A') <
        // glyph_rank('Z')), which only happens if commit() re-sorts.
        let cfg = HsmmConfig::default();
        let token1 = Token {
            node: 0,
            phase: Phase::AfterSpace,
            u: 13.0,
            score: 5.0,
            hist: vec![
                entry(Some(Glyph::Char('A')), 100, 1, None),
                entry(Some(Glyph::Char('Z')), 200, 2, None),
            ],
            anchor_hop: 2,
        };
        let token2 = Token {
            node: 0,
            phase: Phase::AfterSpace,
            u: 13.0,
            score: 5.0, // tied with token1
            hist: vec![
                entry(Some(Glyph::Char('B')), 100, 1, None),
                entry(Some(Glyph::Char('A')), 250, 2, None),
            ],
            anchor_hop: 2,
        };
        // Pre-strip order is already correct per token_order (token1's
        // [A, Z] sorts ahead of token2's [B, A]) -- exactly what a real
        // caller's `cands.sort_by(token_order)` would have produced.
        let mut tokens = vec![token1, token2];
        let mut sealed = vec![(100, true)]; // both leading entries already sealed
        let now_hop = 1000; // forces a commit regardless of consensus
        let out = commit(&mut tokens, now_hop, &cfg, &mut sealed);
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
            let mut hist = vec![entry(Some(Glyph::Char('T')), 512, 100, *glyph_seg)];
            if let Some(bs) = boundary_seg {
                hist.push(entry(None, 512, 100, Some(*bs)));
            }
            let hist_len = hist.len();
            let mut tokens = vec![token(hist, 10.0)];
            let mut sealed = Vec::new();
            let out = commit_outcome(&mut tokens, 100, &cfg, &mut sealed);
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
            vec![
                entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::CGap)),
                entry(None, 512, 100, None),
            ],
            10.0,
        )];
        let mut sealed = Vec::new();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut sealed);
        assert_eq!(out.entries.len(), 2);
        assert!(!out.committed_silence, "a None-provenance boundary must not flag");
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
                entry(Some(Glyph::Char('T')), 512, 100, Some(seg)),
                entry(None, 512, 100, Some(seg)),
            ]
        };
        // Consensus path: both tokens agree on the glyphs; the higher
        // score sorts first and its provenance wins. A losing Silence
        // hypothesis must NOT cause the signal.
        let mut tokens = vec![token(hist(SegType::WGap), 10.0), token(hist(SegType::Silence), 9.0)];
        let mut sealed = Vec::new();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut sealed);
        assert_eq!(out.entries.len(), 2, "consensus commits glyph then boundary");
        assert!(!out.committed_silence, "the winning head closed via WGap");

        let mut tokens = vec![token(hist(SegType::WGap), 9.0), token(hist(SegType::Silence), 10.0)];
        let mut sealed = Vec::new();
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut sealed);
        assert!(
            out.committed_silence,
            "the winning head closed via Silence -- even though the loser closed via WGap"
        );

        // Forced path: tokens disagree on the glyph; only the far-past
        // born_hop forces emission, and the winner's provenance still
        // decides.
        let disagree: Vec<HistEntry> = vec![entry(Some(Glyph::Char('E')), 300, 1, Some(SegType::WGap))];
        let mut tokens = vec![token(hist(SegType::Silence), 10.0), token(disagree, 9.5)];
        let mut sealed = Vec::new();
        let out = commit_outcome(&mut tokens, 100_000, &cfg, &mut sealed);
        assert_eq!(out.entries.len(), 2, "forced commit emits the winner's entries");
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
            entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
            entry(None, 512, 100, Some(SegType::Silence)),
        ];
        let cgap_hist = vec![entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::CGap))];
        let mut tokens = vec![token(silence_hist, 10.0), token(cgap_hist, 9.0)];
        let mut sealed = Vec::new();
        // First call, not forced: the glyph reaches consensus, the boundary
        // does not (the CGap runner-up disagrees by having no entry).
        let out = commit_outcome(&mut tokens, 100, &cfg, &mut sealed);
        assert_eq!(
            out.entries.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('T'))],
            "only the glyph commits on the first call"
        );
        assert!(!out.committed_silence, "no boundary was emitted yet");
        // Second call: the runner-up's glyph entry is now sealed (same
        // sample_ts, is_char) and stripped; force the still-pending
        // boundary with a far-future now_hop.
        let out = commit_outcome(&mut tokens, 100_000, &cfg, &mut sealed);
        assert_eq!(
            out.entries.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![None],
            "the boundary commits on the second call"
        );
        assert!(out.committed_silence, "the boundary head closed via Silence");
        // Third call: re-deriving the already-decided boundary yields no
        // entries and no signal.
        let mut stale = vec![token(
            vec![
                entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
                entry(None, 512, 100, Some(SegType::Silence)),
            ],
            10.0,
        )];
        let out = commit_outcome(&mut stale, 200_000, &cfg, &mut sealed);
        assert!(out.entries.is_empty(), "an already-decided boundary re-emits nothing");
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
                    vec![
                        entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::Silence)),
                        entry(None, 512, 100, Some(SegType::Silence)),
                    ],
                    10.0,
                ),
                token(
                    vec![
                        entry(Some(Glyph::Char('T')), 512, 100, Some(SegType::WGap)),
                        entry(None, 512, 100, Some(SegType::WGap)),
                    ],
                    7.0,
                ),
            ]
        };
        let mut tokens_a = make();
        let mut sealed_a = Vec::new();
        let via_wrapper = commit(&mut tokens_a, 100, &cfg, &mut sealed_a);
        let mut tokens_b = make();
        let mut sealed_b = Vec::new();
        let via_helper = commit_outcome(&mut tokens_b, 100, &cfg, &mut sealed_b);
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
        assert_eq!(sealed_a, sealed_b, "both paths seal identically");
    }
}
