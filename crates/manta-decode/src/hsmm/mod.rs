//! Hidden semi-Markov token-passing Morse decoder. SPEC v2 §4.
pub mod commit;
pub mod segments;
pub mod token;

use crate::evidence::HopEvidence;
use crate::tree::{Glyph, MorseTree};
use segments::SegType;
use std::collections::VecDeque;
use token::{merge_key, token_order, Phase, Token};

#[derive(Debug, Clone)]
pub struct HsmmConfig {
    pub dur_sigma: f32,
    pub mark_insert_penalty: f32,
    pub beam: usize,
    pub lookahead_dits: f32,
    pub speed_alpha: f32,
    pub seed_units_hops: Vec<f32>,
    pub conf_kappa: f32,
    pub u_min: f32,
    pub u_max: f32,
}

impl Default for HsmmConfig {
    fn default() -> Self {
        HsmmConfig {
            dur_sigma: 0.22,
            mark_insert_penalty: -1.5,
            beam: 12,
            lookahead_dits: 25.0,
            speed_alpha: 0.2,
            seed_units_hops: vec![9.0, 13.0, 18.0, 26.0, 38.0],
            conf_kappa: 6.0,
            u_min: 7.5,
            u_max: 56.0,
        }
    }
}

/// One committed decoder entry: a glyph (character/prosign) or a word
/// boundary (`glyph: None`), with confidence and up to two alternatives.
/// SPEC v2 §4.8–4.9.
#[derive(Debug, Clone)]
pub struct Committed {
    pub glyph: Option<Glyph>,
    pub sample_ts: u64,
    pub confidence: f32,
    pub alternatives: Vec<(Glyph, f32)>,
}

struct Anchor {
    hop: u64,
    sample_ts: u64,
    prefix: f64,
    tokens: Vec<Token>,
    /// [Task 8 fix, review round 3] The minimum `HistEntry::sample_ts`
    /// across every token's `hist` in this anchor, computed once at
    /// creation. See `HsmmDecoder::push`'s `sealed`-pruning comment: this
    /// is *not* the same as this anchor's own `sample_ts` -- `successor`
    /// clones an ancestor's whole `hist` and only stamps the *new* entry
    /// with the current anchor's timestamp, so an inherited older entry
    /// (originally stamped by a since-evicted ancestor anchor) can still
    /// be sitting in a still-live anchor's frozen tokens. `None` if no
    /// token in this anchor has any hist entry yet.
    min_hist_ts: Option<u64>,
}

fn min_hist_ts(tokens: &[Token]) -> Option<u64> {
    tokens
        .iter()
        .flat_map(|t| t.hist.iter())
        .map(|e| e.sample_ts)
        .min()
}

/// Token-passing HSMM decoder: consumes per-hop evidence, maintains a beam
/// of speed/position hypotheses anchored at edge candidates, and commits
/// glyphs/word-boundaries via partial traceback. SPEC v2 §4.
pub struct HsmmDecoder {
    cfg: HsmmConfig,
    anchors: VecDeque<Anchor>,
    present: bool,
    /// Tokens alive at the most recent anchor (also stored in anchors.back()).
    live: Vec<Token>,
    /// MAN-192 scored-prefix invariant: `live[..scored_live_len]` is the
    /// sorted surviving candidate beam from the most recent productive
    /// anchor (every one of those tokens has processed at least one real
    /// segment, whatever its score happens to be), and any suffix
    /// `live[scored_live_len..]` consists solely of raw, unscored seeds
    /// appended by the most recent reseed (gate rise or committed
    /// Silence). Zero score alone no longer identifies a seed: a real
    /// token can legitimately sit at exactly 0.0 (e.g. after the uniform
    /// normalization that accompanies reseeding), so speed feedback
    /// (`best_u`) and the EOF flush must use this count, not a score
    /// heuristic.
    scored_live_len: usize,
    hop: u64,
    last_prefix: f64,
    last_ts: u64,
    /// [Task 8 fix; round 6 changed the value to `is_char` rather than the
    /// specific `Glyph`] `(sample_ts, is_char)` pairs ever actually
    /// committed; see `commit::commit`'s doc comment for why this must be
    /// keyed on `sample_ts` (not `born_hop`), why the value is the entry
    /// KIND rather than its exact value, and why it's threaded through
    /// every call.
    sealed: Vec<(u64, bool)>,
}

impl HsmmDecoder {
    pub fn new(cfg: HsmmConfig) -> Self {
        HsmmDecoder {
            cfg,
            anchors: VecDeque::new(),
            present: false,
            live: Vec::new(),
            scored_live_len: 0,
            hop: 0,
            last_prefix: 0.0,
            last_ts: 0,
            sealed: Vec::new(),
        }
    }

    /// The best live token's tracked speed (hops/dit), for `Evidence::set_u_ref`
    /// and WPM reporting. `None` before the first productive anchor, and
    /// whenever the live beam is nothing but raw seeds (immediately after
    /// either reseed path: a keying re-onset, or MAN-192's committed
    /// Silence). Reporting a raw seed's speed here fed `set_u_ref` an
    /// implausible zero-evidence WPM (always `seed_units_hops[0]`, 9.0
    /// hops = 50 WPM) on every real inter-word gap -- the historical
    /// spurious-`SpeedUpdate` bug. MAN-192 replaced the old "skip tokens
    /// that look exactly like a fresh seed" score/history heuristic with
    /// the explicit `scored_live_len` prefix: `live[0]` is a genuinely
    /// scored survivor whenever the count is positive, however low (or
    /// exactly zero) its score, so a normalized real root is never
    /// mistaken for a seed and a raw seed is never mistaken for a root.
    pub fn best_u(&self) -> Option<f32> {
        if self.scored_live_len == 0 {
            return None;
        }
        self.live.first().map(|t| t.u)
    }

    fn seed(&mut self, hop: u64) -> Vec<Token> {
        self.cfg
            .seed_units_hops
            .iter()
            .map(|&u| Token {
                node: MorseTree::ROOT,
                phase: Phase::AfterSpace,
                u,
                score: 0.0,
                hist: Vec::new(),
                anchor_hop: hop,
            })
            .collect()
    }

    /// Shift every retained anchor token's score down by `offset`. Shared
    /// by both reseeding paths (gate rise, and MAN-192's committed
    /// Silence): a uniform constant shift leaves `successor`'s cumulative
    /// sums and `margin`'s score differences -- hence every relative
    /// ordering, tie-break and confidence -- exactly unchanged, while
    /// putting surviving old hypotheses on the same zero baseline as the
    /// fresh seeds they must compete with fairly.
    fn shift_anchor_scores(&mut self, offset: f32) {
        if offset != 0.0 {
            for a in self.anchors.iter_mut() {
                for t in a.tokens.iter_mut() {
                    t.score -= offset;
                }
            }
        }
    }

    /// Feed one evidence hop; returns committed entries (glyph or word
    /// boundary) with confidence and alternatives.
    pub fn push(&mut self, ev: &HopEvidence) -> Vec<Committed> {
        self.hop = ev.hop;
        self.last_prefix = ev.prefix;
        self.last_ts = ev.sample_ts;
        let mut out = Vec::new();
        if ev.present && !self.present {
            // Codex review, PR #161 round 5: `self.anchors` is deliberately
            // NOT cleared on reseed (SPEC v2 §4.6's `reach_sil` retention
            // exists precisely so a short intervening blip doesn't wrongly
            // read as silence) -- so an old anchor's tokens, carrying a
            // cumulative log-likelihood score that can have grown by
            // hundreds to thousands of points over a real decode, remain
            // in the beam right alongside the fresh seeds this branch is
            // about to create at a literal `score: 0.0`. The very next
            // merge/prune step (`token_order`, raw score desc) then keeps
            // whichever token has the highest absolute score regardless of
            // fit -- an old, merely-still-viable hypothesis can dominate a
            // brand new, better-fitting reseed purely on accumulated
            // magnitude, defeating the whole point of reseeding on a real
            // speed change or post-fade restart.
            //
            // Shift every already-live score DOWN by the current best
            // score before adding the zero-baseline seeds, rather than
            // raising the seeds up to meet it: this keeps every existing
            // token's RELATIVE ordering (and every already-computed
            // HistEntry) exactly unchanged -- successor()'s cumulative sum
            // and `margin()`'s score DIFFERENCES are invariant under a
            // constant shift applied uniformly -- while putting the best
            // surviving old hypothesis on the SAME footing (score 0.0) as
            // the fresh seeds it must now compete against fairly. Left
            // `self.live`'s own scores untouched here since `self.live` is
            // overwritten with `seeds` two lines below regardless; only
            // `self.anchors` (what the NEXT anchor step's candidate
            // generation actually reads) needs the shift.
            let offset = self.live.first().map(|t| t.score).unwrap_or(0.0);
            self.shift_anchor_scores(offset);
            let seeds = self.seed(ev.hop);
            self.anchors.push_back(Anchor {
                hop: ev.hop,
                sample_ts: ev.sample_ts,
                prefix: ev.prefix,
                min_hist_ts: min_hist_ts(&seeds),
                tokens: seeds.clone(),
            });
            self.live = seeds;
            self.scored_live_len = 0;
        }
        self.present = ev.present;
        if !ev.anchor || self.anchors.is_empty() {
            return out;
        }
        let tree = MorseTree::shared();
        let u_max = self.live.iter().map(|t| t.u).fold(self.cfg.u_min, f32::max);
        let reach_norm = (1.5 * 7.0 * u_max).ceil() as u64;
        let reach_sil = (80.0 * u_max).ceil() as u64;
        // Codex review, PR #161 round 21: prune stale anchors HERE, before
        // candidate generation, rather than only after a successful
        // commit further down. `cands.is_empty()` below returns early
        // whenever `present` keeps rising but no candidate ever survives
        // (e.g. brief decoder-gate bursts spaced farther apart than
        // reach_norm/reach_sil) -- that early return used to skip the
        // only age-based pruning of `self.anchors`, so a long-lived noisy
        // track could accumulate anchors without bound, and every later
        // anchor step would scan the whole, ever-growing collection. This
        // check depends only on `ev.hop` and `reach_sil` (both already
        // known here, unrelated to whether any candidate is produced this
        // hop), so hoisting it changes nothing about which anchors get
        // pruned on the normal path -- it just also runs on the early
        // return.
        while let Some(front) = self.anchors.front() {
            if ev.hop - front.hop > reach_sil {
                self.anchors.pop_front();
            } else {
                break;
            }
        }
        let mut cands: Vec<Token> = Vec::new();
        for a in self.anchors.iter() {
            let d = ev.hop - a.hop;
            if d == 0 || d > reach_sil {
                continue;
            }
            let evidence = (ev.prefix - a.prefix) as f32;
            for tok in &a.tokens {
                for seg in SegType::ALL {
                    if seg == SegType::Silence {
                        if d < (10.0 * tok.u) as u64 {
                            continue;
                        }
                    } else if d > reach_norm {
                        continue;
                    }
                    if let Some(s) = tok.successor(
                        seg,
                        d as u32,
                        evidence,
                        tree,
                        &self.cfg,
                        ev.hop,
                        a.sample_ts,
                    ) {
                        cands.push(s);
                    }
                }
            }
        }
        if cands.is_empty() {
            return out;
        }
        // Merge by (node, phase, round(2u)): keep the best by token_order.
        cands.sort_by(token_order);
        let mut merged: Vec<Token> = Vec::with_capacity(cands.len());
        'outer: for c in cands {
            for m in &merged {
                if merge_key(m) == merge_key(&c) {
                    continue 'outer;
                }
            }
            merged.push(c);
        }
        merged.truncate(self.cfg.beam);
        // Commit consensus / forced entries. MAN-192: the internal helper
        // also reports whether this step emitted a winning word-boundary
        // head closed by `Silence` -- SPEC v2 §4.7's second seeding
        // trigger, beside the gate rise above.
        let outcome = commit::commit_outcome(&mut merged, ev.hop, &self.cfg, &mut self.sealed);
        out.extend(outcome.entries);
        self.scored_live_len = merged.len();
        if outcome.committed_silence && ev.present {
            // MAN-192: a Silence was committed while the keying-present
            // gate stayed asserted (interference held it high through the
            // gap, so the gate-rise branch never fired). Merge the
            // configured seed set so a transmission resuming at a
            // different speed has fresh hypotheses to compete with the
            // surviving root token -- the same round-5 normalization the
            // gate-rise reseed applies, extended to the survivors sitting
            // in `merged` (they are about to become `live` and the next
            // anchor snapshot, so they must share the seeds' baseline
            // too). Seeds are appended AFTER the beam truncation, on
            // purpose: injected as candidates they would be pruned before
            // any evidence could act, and §4.7's "the first character's
            // evidence prunes them" requires them to survive to take that
            // evidence. The survivors keep their node, phase, speed,
            // history and anchor identity -- only a common score offset
            // changes. When `present` is false, skip the injection per
            // §1.4 (no seeding while the gate is low); the subsequent
            // gate rise supplies the seeds.
            let offset = merged.first().map(|t| t.score).unwrap_or(0.0);
            self.shift_anchor_scores(offset);
            if offset != 0.0 {
                for t in merged.iter_mut() {
                    t.score -= offset;
                }
            }
            merged.extend(self.seed(ev.hop));
        }
        self.live = merged.clone();
        // The just-pushed anchor is always age 0 relative to `ev.hop`, so
        // it can never itself need pruning here -- the age-based prune
        // now runs unconditionally earlier in this function (see the
        // round-21 comment above), which already covers this anchor's
        // eventual retirement on some later hop.
        self.anchors.push_back(Anchor {
            hop: ev.hop,
            sample_ts: ev.sample_ts,
            prefix: ev.prefix,
            min_hist_ts: min_hist_ts(&merged),
            tokens: merged,
        });
        // [Task 8 fix, review round 4] Bound `sealed`'s growth: the safe
        // prune bound is the minimum `sample_ts` over BOTH (a) every hist
        // entry of every token in every currently-live anchor (round 3's
        // `min_hist_ts`, needed because `Token::successor` clones an
        // ancestor's whole `hist` and only stamps the *new* entry with the
        // current anchor's own timestamp, so an inherited older entry can
        // outlive the ancestor anchor that originally produced it) AND (b)
        // every live anchor's own `sample_ts` (round 3 missed this half:
        // `successor` can also stamp a *brand new* hist entry at a live
        // anchor's own `sample_ts` via `seg_start_ts = a.sample_ts` --  a
        // value that isn't in anyone's `hist` yet, so no `min_hist_ts`
        // reflects it; worse, an anchor whose tokens' hist was just fully
        // drained by a commit reports `min_hist_ts == None`, and round 3's
        // `None -> clear()` fallback would discard every real seal once
        // every live anchor was in that state). `anchors` is FIFO by hop
        // and `sample_ts` is monotone in hop, so `front.sample_ts` is
        // exactly the min over every live anchor's own timestamp -- no
        // need to scan all anchors for that half.
        let hist_bound = self.anchors.iter().filter_map(|a| a.min_hist_ts).min();
        let front_ts = self.anchors.front().map(|a| a.sample_ts);
        match (hist_bound, front_ts) {
            (Some(h), Some(f)) => self.sealed.retain(|&(ts, _)| ts >= h.min(f)),
            (Some(h), None) => self.sealed.retain(|&(ts, _)| ts >= h),
            (None, Some(f)) => self.sealed.retain(|&(ts, _)| ts >= f),
            (None, None) => self.sealed.clear(),
        }
        out
    }

    /// End of stream: flush any remaining committable history from the best
    /// surviving token.
    pub fn finish(&mut self) -> Vec<Committed> {
        let mut out = Vec::new();
        // MAN-192: drop the raw-seed suffix first -- an empty-history seed
        // sorts before a normalized zero-score incumbent that carries
        // pending history, so leaving the suffix in would let a never-
        // scored seed win the EOF flush, displacing real pending text and
        // depressing its confidence with a bogus `s_alt`.
        self.live.truncate(self.scored_live_len);
        if self.live.is_empty() {
            self.scored_live_len = 0;
            return out;
        }
        // [Task 8 fix] Same stale-anchor concern as `commit::commit`: strip
        // anything this decoder already committed before reading off the
        // tail, so a straggler token cloned from an older, un-trimmed
        // anchor snapshot can't resurface an already-emitted entry here.
        //
        // Codex review, PR #161 round 7: the old `while ... hist.first()
        // ...` loop only ever stripped a leading, CONTIGUOUS run of sealed
        // entries -- correct for `commit::commit`'s own use (that
        // function re-checks the front on every outer-loop pass, so a
        // newly-revealed sealed front is caught next iteration -- and it
        // never reads past the front in a single pass anyway), but wrong
        // here, where this check runs only once before reading the WHOLE
        // history. A stale-anchor token can hold [an uncommitted entry,
        // ..., an entry some OTHER lineage already committed and sealed]
        // -- sealing is keyed globally on `(sample_ts, is_char)`, not per
        // token, so a token whose own earlier entry never matched
        // consensus (and so was never stripped) can still carry a LATER
        // entry that happens to match what a different, already-committed
        // lineage sealed. The old front-only check left that buried
        // sealed entry in place, and the loop below would then re-emit it
        // as a duplicate `CharDecoded`/`WordBoundary`, corrupting the
        // final decoded text. `retain` removes every sealed entry
        // anywhere in the history, not just a leading run.
        let sealed = &self.sealed;
        for t in self.live.iter_mut() {
            t.hist
                .retain(|e| !sealed.contains(&(e.sample_ts, e.glyph.is_some())));
        }
        self.live.sort_by(token_order);
        let best = self.live[0].clone();
        let hist = best.hist.clone();
        // Codex review, PR #161 round 9: a live forced commit
        // (`commit::commit`) drops every token that disagreed with the
        // just-committed entry BEFORE scoring the next one
        // (`tokens.retain(...)`), so a rejected runner-up never
        // contributes its score to a LATER entry's margin. Computing
        // every entry's margin against the same static `self.live`
        // instead let a hypothesis rejected at an EARLY entry keep
        // dragging down `s_alt` (and appearing in `alternatives`) for
        // every entry after it, artificially depressing confidence and
        // reported alternatives for characters it never actually
        // competed for -- contaminating downstream spot-confidence and
        // separation measurements. Mirror the same per-entry filtering
        // here on a local copy.
        let mut live = self.live.clone();
        for (j, e) in hist.iter().enumerate() {
            let (confidence, alternatives) = commit::margin(&live, j, &self.cfg);
            out.push(Committed {
                glyph: e.glyph,
                sample_ts: e.sample_ts,
                confidence,
                alternatives,
            });
            live.retain(|t| t.hist.get(j).map(|he| he.glyph) == Some(e.glyph));
        }
        if best.phase == Phase::AfterMark {
            if let Some(g) = MorseTree::shared().glyph(best.node) {
                out.push(Committed {
                    glyph: Some(g),
                    sample_ts: self.last_ts,
                    confidence: 0.5,
                    alternatives: Vec::new(),
                });
            }
        }
        self.live.clear();
        self.scored_live_len = 0;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(hop: u64, present: bool, anchor: bool) -> HopEvidence {
        HopEvidence {
            hop,
            sample_ts: hop * 256,
            llr: if present { 10.0 } else { -10.0 },
            present,
            anchor,
            prefix: if present { hop as f64 * 10.0 } else { 0.0 },
            amp: if present { 1.0 } else { 0.01 },
            mark_level: 1.0,
            noise_level: 0.1,
        }
    }

    #[test]
    fn best_u_ignores_unscored_seeds() {
        // [Task 8 fix, review round 2] Direct, deterministic regression
        // test for the bug described in `best_u`'s doc comment: a reseed
        // (`present` rising edge) resets `self.live` to raw, completely
        // unscored seed tokens (`score == 0.0`, `hist` empty). Before this
        // fix, `best_u()` still returned a speed from `live.first()` in
        // this state -- always `cfg.seed_units_hops[0]` = 9.0 hops (50
        // WPM) -- with zero evidence behind it. Constructing the
        // `HopEvidence` directly (rather than driving this through the
        // full `Evidence`/`NoiseTracker` pipeline, which is noisy during
        // legitimate multi-seed convergence and would conflate this exact,
        // deterministic bug with that unrelated transient behavior) lets
        // this test target precisely the reseed moment.
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        assert_eq!(dec.best_u(), None, "no anchor yet");
        // Rising edge with `anchor: false`: `push` seeds `self.live` and
        // returns before running the anchor/candidate-generation step, so
        // `self.live` is left as the raw, just-created seed set.
        dec.push(&ev(100, true, false));
        assert_eq!(
            dec.best_u(),
            None,
            "must not report a speed from a freshly-seeded, zero-evidence token"
        );
    }

    #[test]
    fn best_u_reports_once_a_token_has_real_evidence() {
        // Companion to `best_u_ignores_unscored_seeds`: once a seed has
        // actually taken a segment transition (real evidence folded into
        // its score), `best_u` must report it again -- the fix must not
        // regress into permanently withholding a speed once one exists.
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        dec.push(&ev(100, true, false)); // reseed: raw seeds, best_u() == None
                                         // A long, unbroken mark: dit_hops = 13 fits several seeds' Dit
                                         // window (u=9 range [5.4,13.5], u=13 range [7.8,19.5]) and is an
                                         // anchor step, so `push` runs candidate generation this time.
        dec.push(&ev(113, true, true));
        assert!(
            dec.best_u().is_some(),
            "a token with real, scored evidence must be reported"
        );
    }

    #[test]
    fn reseed_normalizes_retained_scores_to_the_current_best() {
        // Codex review, PR #161 round 5: `self.anchors` isn't cleared on
        // reseed (SPEC v2 §4.6 retention), so an old anchor's tokens keep
        // whatever cumulative score they'd accumulated before the gap
        // while a fresh reseed starts at a literal 0.0 -- comparing those
        // directly in the next merge/prune step let the old hypothesis's
        // sheer magnitude win regardless of fit, defeating reseeding
        // entirely. Build up a real positive score via an anchor step,
        // force a reseed, then verify every retained old token was
        // shifted down by exactly that amount: the best surviving old
        // token must now read exactly 0.0, the same baseline as the
        // fresh seeds it competes against.
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        dec.push(&ev(100, true, false)); // reseed: raw seeds
        dec.push(&ev(113, true, true)); // real evidence: scores become nonzero

        let pre_reseed_best = dec
            .anchors
            .iter()
            .flat_map(|a| a.tokens.iter())
            .map(|t| t.score)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            pre_reseed_best > 0.0,
            "sanity check: real evidence must produce a positive score, got {pre_reseed_best}"
        );

        dec.push(&ev(114, false, false)); // silence
        dec.push(&ev(115, true, false)); // present rises again: reseed

        let anchors_before_the_fresh_one = dec.anchors.len() - 1;
        let post_reseed_best_retained = dec
            .anchors
            .iter()
            .take(anchors_before_the_fresh_one)
            .flat_map(|a| a.tokens.iter())
            .map(|t| t.score)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (post_reseed_best_retained - 0.0).abs() < 1e-4,
            "the best retained old token must be shifted down to exactly 0.0 (the fresh seeds' \
             own baseline), got {post_reseed_best_retained}"
        );
    }

    #[test]
    fn finish_does_not_reemit_an_entry_sealed_by_a_different_lineage() {
        // Codex review, PR #161 round 7: sealing is keyed globally on
        // (sample_ts, is_char), not per token, so a stale-anchor token can
        // hold an uncommitted entry followed by a LATER entry some other
        // lineage already committed and sealed. The old front-only strip
        // in `finish()` missed this buried sealed entry and would re-emit
        // it as a duplicate CharDecoded.
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        dec.sealed.push((200, true)); // a different lineage already committed a char at ts=200
        dec.live = vec![Token {
            node: MorseTree::ROOT,
            phase: Phase::AfterSpace,
            u: 13.0,
            score: 1.0,
            hist: vec![
                token::HistEntry {
                    glyph: Some(Glyph::Char('A')),
                    sample_ts: 100,
                    born_hop: 5,
                    closing_segment: None,
                },
                token::HistEntry {
                    glyph: Some(Glyph::Char('B')),
                    sample_ts: 200,
                    born_hop: 10,
                    closing_segment: None,
                },
            ],
            anchor_hop: 10,
        }];
        dec.scored_live_len = dec.live.len(); // MAN-192: the fixture's live is all scored
        let out = dec.finish();
        assert!(
            !out.iter().any(|c| c.sample_ts == 200),
            "must not re-emit the entry at sample_ts=200, already sealed by a different \
             lineage: {out:?}"
        );
        assert!(
            out.iter().any(|c| c.sample_ts == 100),
            "the genuinely uncommitted entry at sample_ts=100 must still be emitted: {out:?}"
        );
    }

    #[test]
    fn finish_does_not_let_an_early_rejected_hypothesis_depress_a_later_entrys_confidence() {
        // Codex review, PR #161 round 9: a live forced commit drops a
        // disagreeing runner-up before scoring the next entry -- a
        // rejected hypothesis must not keep depressing confidence for
        // entries after the one it disagreed on.
        let cfg = HsmmConfig::default();
        let best_hist = vec![
            token::HistEntry {
                glyph: Some(Glyph::Char('A')),
                sample_ts: 100,
                born_hop: 1,
                closing_segment: None,
            },
            token::HistEntry {
                glyph: Some(Glyph::Char('B')),
                sample_ts: 200,
                born_hop: 2,
                closing_segment: None,
            },
        ];
        let best_token = |hist: Vec<token::HistEntry>| Token {
            node: MorseTree::ROOT,
            phase: Phase::AfterSpace,
            u: 13.0,
            score: 10.0,
            hist,
            anchor_hop: 2,
        };

        let mut dec_baseline = HsmmDecoder::new(cfg.clone());
        dec_baseline.live = vec![best_token(best_hist.clone())];
        dec_baseline.scored_live_len = 1; // MAN-192: scored-only control
        let out_baseline = dec_baseline.finish();

        let mut dec_with_rejected = HsmmDecoder::new(cfg);
        dec_with_rejected.live = vec![
            best_token(best_hist),
            Token {
                node: MorseTree::ROOT,
                phase: Phase::AfterSpace,
                u: 13.0,
                score: 9.5, // close, but loses token_order to the best token
                hist: vec![token::HistEntry {
                    glyph: Some(Glyph::Char('C')), // disagrees at entry 0 only
                    sample_ts: 100,
                    born_hop: 1,
                    closing_segment: None,
                }],
                anchor_hop: 1,
            },
        ];
        dec_with_rejected.scored_live_len = dec_with_rejected.live.len(); // MAN-192
        let out_with_rejected = dec_with_rejected.finish();

        let conf_baseline_1 = out_baseline[1].confidence;
        let conf_with_rejected_1 = out_with_rejected[1].confidence;
        assert!(
            (conf_baseline_1 - conf_with_rejected_1).abs() < 1e-5,
            "entry 1's confidence must be unaffected by a runner-up that only disagreed at \
             entry 0 (baseline {conf_baseline_1}, with-rejected {conf_with_rejected_1})"
        );
    }

    #[test]
    fn push_prunes_stale_anchors_even_when_no_candidate_survives() {
        // Codex review, PR #161 round 21: `cands.is_empty()` used to
        // return early WITHOUT pruning stale anchors -- when `present`
        // keeps rising but no candidate ever survives (brief bursts
        // spaced farther apart than reach_norm/reach_sil), a long-lived
        // noisy track could accumulate anchors without bound. Construct
        // an anchor old enough that no candidate can ever be generated
        // from it (d > reach_sil), verify it gets pruned anyway.
        let cfg = HsmmConfig::default();
        let mut dec = HsmmDecoder::new(cfg);
        dec.push(&ev(0, true, false)); // reseed: one anchor at hop 0
        assert_eq!(
            dec.anchors.len(),
            1,
            "sanity: one anchor after the first reseed"
        );

        // Far enough forward that this anchor is now older than reach_sil
        // (80 * u_max, u_max = 38.0 for the default largest seed unit ->
        // reach_sil = 3040) -- `present` stays true, so no new reseed
        // fires here.
        let far_hop = 100_000u64;
        let out = dec.push(&ev(far_hop, true, true));
        assert!(
            out.is_empty(),
            "sanity: expected no commits from a candidate-free anchor step, got {out:?}"
        );
        assert!(
            dec.anchors.is_empty(),
            "the stale anchor from hop 0 must be pruned even though no candidate survived \
             this far-future anchor step, got anchors at hops {:?}",
            dec.anchors.iter().map(|a| a.hop).collect::<Vec<_>>()
        );
    }

    /// MAN-192 evidence builder: like `ev` but with an explicit `prefix`
    /// (the running mark-evidence sum), so a fixture can freeze evidence
    /// across a gate-stuck gap exactly the way a real long gap does.
    fn man192_ev(hop: u64, present: bool, anchor: bool, prefix: f64) -> HopEvidence {
        HopEvidence {
            hop,
            sample_ts: hop * 256,
            llr: if present { 10.0 } else { -10.0 },
            present,
            anchor,
            prefix,
            amp: if present { 1.0 } else { 0.01 },
            mark_level: 1.0,
            noise_level: 0.1,
        }
    }

    fn man192_cfg(seeds: &[f32]) -> HsmmConfig {
        HsmmConfig {
            seed_units_hops: seeds.to_vec(),
            ..HsmmConfig::default()
        }
    }

    /// MAN-192: the ticket's scenario. A 13-hop mark leaves speed
    /// hypotheses 9.8/13.0; the 137-hop gap that follows is outside every
    /// WGap window (max `1.5 * 7 * 13 = 136.5`) and inside both Silence
    /// windows, `present` never drops, and the committed boundary must
    /// merge the configured seed set into the live beam (SPEC v2 §4.7).
    /// On the pre-fix code the E and word boundary commit but no seed is
    /// ever added -- this is the red/green oracle.
    #[test]
    fn man192_committed_silence_with_gate_high_merges_seeds() {
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        let out = dec.push(&man192_ev(100, true, true, 0.0));
        assert!(out.is_empty(), "seed anchor alone commits nothing: {out:?}");
        let out = dec.push(&man192_ev(113, true, true, 130.0));
        assert!(
            out.is_empty(),
            "the mark is mid-character, no commit yet: {out:?}"
        );
        let out = dec.push(&man192_ev(250, true, true, 130.0)); // frozen gap, gate high
        assert_eq!(
            out.iter()
                .map(|c| (c.glyph, c.sample_ts))
                .collect::<Vec<_>>(),
            vec![(Some(Glyph::Char('E')), 113 * 256), (None, 113 * 256)],
            "the Silence close must commit the E glyph and a word boundary at the mark's \
             anchor ts: {out:?}"
        );
        // Raw seeds are identified by the scored-prefix invariant, NOT by
        // zero score: the normalized winning root also sits at exactly
        // 0.0 with empty history (its entries were just committed and
        // stripped), and must not be counted as (or mistaken for) a seed.
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(
            raw,
            vec![9.0, 13.0],
            "SPEC v2 §4.7 violated: the configured seed set must be merged into live after \
             the committed Silence"
        );
        assert_eq!(dec.scored_live_len, 2, "both Silence survivors stay scored");
        assert!(dec.scored_live_len <= dec.cfg.beam);
        assert!(
            dec.live.len() <= dec.cfg.beam + dec.cfg.seed_units_hops.len(),
            "snapshot bound: beam + one seed set"
        );
        // The current anchor snapshot is published from the same vector,
        // so the seeds are visible to the next anchor's candidate
        // generation (frozen in anchors.back(), age 0).
        let back = dec.anchors.back().unwrap();
        assert_eq!(back.hop, 250);
        assert_eq!(
            back.tokens.iter().map(|t| t.u).collect::<Vec<_>>(),
            dec.live.iter().map(|t| t.u).collect::<Vec<_>>(),
            "the anchor snapshot must equal live, seeds included"
        );
        // Speed feedback reads the scored prefix: the surviving root (u
        // 13.0 lineage won the sort), never the first raw seed (9.0).
        assert_eq!(dec.best_u(), Some(13.0));
    }

    /// MAN-192: with the full default seed set the same recovery must
    /// work, and a nondefault configured speed must actually be able to
    /// compete after rejoining -- the point of reseeding is not the
    /// tokens, it is the resumed decode at a new speed.
    #[test]
    fn man192_default_set_reseeds_and_a_nondefault_seed_competes() {
        // The default five seeds plus a sixth (24.0 hops) that is not any
        // speed the pre-gap beam could reach (the survivors sit at u 13
        // and 17 after the 13-hop mark).
        let seeds = vec![9.0, 13.0, 18.0, 26.0, 38.0, 24.0];
        let mut dec = HsmmDecoder::new(man192_cfg(&seeds));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        // A gap of 887 hops from the mark anchor: past every WGap window
        // of the surviving {9.8, 13, 17} lineages (max 1.5*7*17 = 178.5),
        // inside the u=13/17 Silence windows, and past every mark window
        // of the raw seeds at anchor 100 (max 1.5*3*38 = 171), so the
        // winning boundary is provably Silence-origin.
        let out = dec.push(&man192_ev(1000, true, true, 130.0));
        assert!(
            out.iter().any(|c| c.glyph.is_none()),
            "a word boundary must commit: {out:?}"
        );
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(raw, seeds, "all six configured seeds, in configured order");
        // A 24-hop mark at the next anchor: a valid Dit exactly at the
        // fresh u=24 hypothesis' nominal, so that seed takes a real
        // successor and enters the scored beam.
        let out = dec.push(&man192_ev(1024, true, true, 130.0 + 240.0));
        assert!(out.is_empty(), "mid-character mark: {out:?}");
        assert!(
            dec.live[..dec.scored_live_len]
                .iter()
                .any(|t| (t.u - 24.0).abs() < 1e-4),
            "the fresh u=24 hypothesis must be eligible to compete in the scored beam: {:?}",
            dec.live[..dec.scored_live_len]
                .iter()
                .map(|t| t.u)
                .collect::<Vec<_>>()
        );
    }

    /// MAN-192: the reseed's uniform score translation must shift every
    /// retained anchor and every current survivor by exactly one offset,
    /// preserving pairwise differences and every non-score field, with the
    /// surviving root kept alongside the seeds.
    #[test]
    fn man192_silence_reseed_shifts_all_scores_by_one_offset() {
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        // Snapshot the retained anchors (100, 113) before the commit hop.
        let before: Vec<(u64, Vec<(f32, _, token::Phase, f32, usize)>)> = dec
            .anchors
            .iter()
            .map(|a| {
                (
                    a.hop,
                    a.tokens
                        .iter()
                        .map(|t| (t.score, t.node, t.phase, t.u, t.hist.len()))
                        .collect(),
                )
            })
            .collect();
        let out = dec.push(&man192_ev(250, true, true, 130.0));
        assert!(!out.is_empty());
        // The fresh anchor (250) is new; the retained ones must still be
        // there, each token shifted by one common offset.
        let mut offsets: Vec<f32> = Vec::new();
        for (hop, tokens_before) in &before {
            let a = dec
                .anchors
                .iter()
                .find(|a| &a.hop == hop)
                .unwrap_or_else(|| panic!("anchor {hop} must be retained"));
            assert_eq!(a.tokens.len(), tokens_before.len(), "anchor {hop}");
            for (t, (score, node, phase, u, hist_len)) in a.tokens.iter().zip(tokens_before) {
                assert_eq!(t.node, *node, "identity fields must not change");
                assert_eq!(t.phase, *phase);
                assert!((t.u - u).abs() < 1e-6);
                assert_eq!(t.hist.len(), *hist_len, "history must be untouched");
                offsets.push(*score - t.score);
            }
        }
        let offset = offsets[0];
        assert!(
            offsets.iter().all(|o| (o - offset).abs() < 1e-3),
            "every retained token must shift by the same offset, got {offsets:?}"
        );
        // The scored survivors now in live were shifted by that same
        // offset (pairwise differences against the retained beam hold),
        // and the best survivor sits at exactly the seeds' 0.0 baseline.
        assert_eq!(
            dec.live[0].score, 0.0,
            "the best survivor lands on the seed baseline"
        );
        assert!(
            dec.scored_live_len >= 1,
            "the surviving root stays in the beam"
        );
        assert_eq!(
            dec.live[dec.scored_live_len..].len(),
            2,
            "alongside the seed set"
        );
    }

    /// MAN-192: `best_u` follows the scored prefix, not a score heuristic.
    /// A real root that processed a segment can legitimately sit at
    /// exactly 0.0 with empty history (that is what the reseed
    /// normalization produces); it must still report its speed, while a
    /// genuinely raw seed beam still reports `None`.
    #[test]
    fn man192_best_u_reports_a_zero_score_scored_root() {
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        dec.live = vec![
            Token {
                node: MorseTree::ROOT,
                phase: Phase::AfterSpace,
                u: 13.0, // the winning root: real, scored, exactly zero
                score: 0.0,
                hist: Vec::new(),
                anchor_hop: 250,
            },
            Token {
                node: MorseTree::ROOT,
                phase: Phase::AfterSpace,
                u: 9.8, // runner-up with pending history
                score: -1.0,
                hist: vec![token::HistEntry {
                    glyph: Some(Glyph::Char('E')),
                    sample_ts: 28_928,
                    born_hop: 250,
                    closing_segment: Some(SegType::Silence),
                }],
                anchor_hop: 250,
            },
        ];
        dec.scored_live_len = 2;
        assert_eq!(
            dec.best_u(),
            Some(13.0),
            "a scored zero-score empty-history root must keep reporting its speed"
        );
        // The old heuristic returned Some(9.8) here -- exactly the
        // normalized-root failure the plan reproduced.
        // Raw seeding still reports None.
        let mut fresh = HsmmDecoder::new(HsmmConfig::default());
        fresh.push(&man192_ev(100, true, false, 0.0));
        assert_eq!(fresh.best_u(), None, "a raw seed beam reports no speed");
    }

    /// MAN-192: the EOF flush must ignore appended raw seeds -- they can
    /// neither win the final sort over a normalized incumbent with pending
    /// history nor depress its confidence.
    #[test]
    fn man192_finish_ignores_raw_seed_suffix() {
        let incumbent = || Token {
            node: MorseTree::ROOT,
            phase: Phase::AfterSpace,
            u: 13.0,
            score: 0.0, // normalized: a plausible real score of exactly zero
            hist: vec![token::HistEntry {
                glyph: Some(Glyph::Char('E')),
                sample_ts: 100,
                born_hop: 5,
                closing_segment: Some(SegType::CGap),
            }],
            anchor_hop: 5,
        };
        // Scored-only control.
        let mut control = HsmmDecoder::new(HsmmConfig::default());
        control.live = vec![incumbent()];
        control.scored_live_len = 1;
        let out_control = control.finish();
        // Same incumbent with the default five raw seeds appended.
        let mut with_seeds = HsmmDecoder::new(HsmmConfig::default());
        with_seeds.live = vec![incumbent()];
        let suffix = with_seeds.seed(250);
        with_seeds.live.extend(suffix);
        with_seeds.scored_live_len = 1;
        let out_seeds = with_seeds.finish();
        assert_eq!(out_control.len(), 1);
        assert_eq!(out_seeds.len(), 1, "seeds must add no EOF entries");
        assert_eq!(
            out_control[0].confidence.to_bits(),
            out_seeds[0].confidence.to_bits(),
            "seeds must not depress the incumbent's confidence"
        );
        assert_eq!(out_control[0].glyph, out_seeds[0].glyph);
        assert_eq!(out_control[0].sample_ts, out_seeds[0].sample_ts);
        // Seed-only EOF: nothing to flush.
        let mut seed_only = HsmmDecoder::new(HsmmConfig::default());
        let seeds = seed_only.seed(250);
        seed_only.live = seeds;
        seed_only.scored_live_len = 0;
        assert!(
            seed_only.finish().is_empty(),
            "a raw-seed beam flushes nothing"
        );
        // Repeated EOF: the second call is empty.
        let mut twice = HsmmDecoder::new(HsmmConfig::default());
        twice.live = vec![incumbent()];
        twice.scored_live_len = 1;
        assert_eq!(twice.finish().len(), 1);
        assert!(twice.finish().is_empty(), "second finish is empty");
        // No-candidate return: an empty beam flushes nothing.
        let mut empty = HsmmDecoder::new(HsmmConfig::default());
        assert!(empty.finish().is_empty());
    }

    /// MAN-192 negative controls: only a committed Silence boundary with
    /// the gate high reseeds. A committed WGap, a merely generated
    /// (uncommitted) Silence candidate, and a Silence committed while
    /// `present` is false must not.
    #[test]
    fn man192_no_reseed_for_wgap_uncommitted_or_gate_low_silence() {
        // Committed WGap: a 77-hop gap from the mark anchor is inside
        // both survivors' WGap windows and outside every Silence window.
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        let out = dec.push(&man192_ev(190, true, true, 130.0));
        assert_eq!(
            out.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('E')), None],
            "the WGap close commits E plus a boundary: {out:?}"
        );
        assert_eq!(
            dec.live.len(),
            dec.scored_live_len,
            "a committed WGap must not reseed"
        );
        // Merely generated Silence: with the full default set the same
        // 137-hop gap generates Silence candidates, but the u=38 seed's
        // Dah interpretation (window [68.4, 171] covers d=150 from the
        // seed anchor) keeps the beam from agreeing, nothing commits, and
        // nothing reseeds.
        let mut dec = HsmmDecoder::new(HsmmConfig::default());
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        let out = dec.push(&man192_ev(250, true, true, 130.0));
        assert!(out.is_empty(), "no commit happens in this scene: {out:?}");
        assert_eq!(
            dec.live.len(),
            dec.scored_live_len,
            "an uncommitted Silence candidate must not reseed"
        );
        // Silence committed while present=false: the commit machinery is
        // gate-independent, but seeding is not (SPEC v2 §1.4).
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        let out = dec.push(&man192_ev(250, false, true, 130.0));
        assert_eq!(
            out.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('E')), None],
            "the Silence still commits with the gate low: {out:?}"
        );
        assert_eq!(
            dec.live.len(),
            dec.scored_live_len,
            "no seeding while present is false; the later gate rise supplies seeds"
        );
    }

    /// MAN-192: ordinary gate drop/rise behavior is unchanged, and a gate
    /// rise that coincides with a Silence commit on the same hop leaves
    /// exactly one appended seed set in live.
    #[test]
    fn man192_gate_rise_and_silence_commit_on_the_same_hop() {
        // Plain drop/rise across a non-anchor hop: live becomes exactly
        // the raw seed set.
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        dec.push(&man192_ev(200, false, false, 130.0)); // gate drops
        dec.push(&man192_ev(210, true, false, 130.0)); // gate rises
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(raw, vec![9.0, 13.0], "the rise reseeds the configured set");
        assert_eq!(dec.scored_live_len, 0, "all of live is raw seeds");
        assert_eq!(dec.best_u(), None);
        // Rise + Silence commit on the same hop: the gate-rise branch
        // fires first, then the anchor step commits the pending Silence
        // and appends one more seed set after the survivors -- the final
        // live vector holds the survivors plus exactly ONE configured set.
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        dec.push(&man192_ev(113, true, true, 130.0));
        dec.push(&man192_ev(200, false, false, 130.0)); // gate drops, no anchor
        let out = dec.push(&man192_ev(250, true, true, 130.0)); // rise + anchor
        assert_eq!(
            out.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('E')), None],
            "the pending Silence commits on the same hop as the rise: {out:?}"
        );
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(raw, vec![9.0, 13.0], "exactly one appended configured set");
        assert_eq!(
            dec.scored_live_len, 2,
            "the Silence survivors lead the beam"
        );
        assert!(dec.live.len() <= dec.cfg.beam + dec.cfg.seed_units_hops.len());
    }

    /// MAN-192: no cooldown. Two distinct eligible Silence boundary
    /// commits 117 hops apart -- closer than `10 * u_max_live` (130 at
    /// u_max 13), the throttle the research proposed and the plan
    /// rejected -- must BOTH reseed. A fast hypothesis legitimately
    /// completes a valid Silence inside that interval.
    #[test]
    fn man192_two_silence_commits_closer_than_the_rejected_cooldown_both_reseed() {
        let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
        dec.push(&man192_ev(100, true, true, 0.0));
        // A 20-hop mark: a Dah for the u=9 seed only (u=13 is in neither
        // window), leaving one T lineage at u ~ 8.53.
        let out = dec.push(&man192_ev(120, true, true, 200.0));
        assert!(out.is_empty(), "mid-character: {out:?}");
        // 97-hop gap: past that lineage's WGap ceiling (1.5*7*8.53 =
        // 89.6), inside its Silence window [85.3, 682.7]. First commit.
        let out = dec.push(&man192_ev(217, true, true, 200.0));
        assert_eq!(
            out.iter().map(|c| c.glyph).collect::<Vec<_>>(),
            vec![Some(Glyph::Char('T')), None],
            "first Silence commit: {out:?}"
        );
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(raw, vec![9.0, 13.0], "first committed Silence reseeds");
        // Mark again (20 hops), then the same gap shape: 237 + 97 = 334,
        // i.e. 117 hops after the first reseed -- inside 10*u_max_live
        // (130) for this beam. Every step is a real keying event.
        let out = dec.push(&man192_ev(237, true, true, 400.0));
        assert!(out.is_empty(), "mid-character: {out:?}");
        let out = dec.push(&man192_ev(334, true, true, 400.0));
        assert_eq!(
            out.iter()
                .map(|c| (c.glyph, c.sample_ts))
                .collect::<Vec<_>>(),
            vec![(Some(Glyph::Char('T')), 237 * 256), (None, 237 * 256)],
            "second distinct Silence commit: {out:?}"
        );
        let raw: Vec<f32> = dec.live[dec.scored_live_len..]
            .iter()
            .map(|t| t.u)
            .collect();
        assert_eq!(
            raw,
            vec![9.0, 13.0],
            "the second commit, 117 hops (< 10*u_max_live) after the first, must reseed too"
        );
    }

    /// MAN-192 phase 3: repeated recovery over a long run. Twelve
    /// mark(20)/gap(97) cycles -- each a distinct eligible Silence
    /// boundary commit on a gate that never drops -- must each reseed;
    /// a subsequent fallback-anchor tail carries the run past
    /// `80 * cfg.u_max` (4480) hops with every snapshot inside the
    /// `beam + seed set` bound, anchors expiring instead of
    /// accumulating, and the whole commit stream byte-identical on
    /// replay.
    #[test]
    fn man192_repeated_recovery_is_bounded_and_deterministic() {
        let build_events = || {
            let mut events = Vec::new();
            events.push(man192_ev(100, true, true, 0.0));
            for k in 0..12u64 {
                let mark_hop = 120 + 117 * k;
                let gap_hop = mark_hop + 97;
                let prefix = 200.0 * (k as f64 + 1.0);
                events.push(man192_ev(mark_hop, true, true, prefix));
                events.push(man192_ev(gap_hop, true, true, prefix));
            }
            // Fallback-anchor tail (every 8 hops, evidence frozen, gate
            // stuck high) past 80*cfg.u_max = 4480 hops from the start.
            let last_gap = 217 + 117 * 11;
            let mut hop = last_gap + 8;
            while hop < 4700 {
                events.push(man192_ev(hop, true, true, 2400.0));
                hop += 8;
            }
            events
        };
        let run = |events: Vec<HopEvidence>| {
            let mut dec = HsmmDecoder::new(man192_cfg(&[9.0, 13.0]));
            let mut commits = Vec::new();
            let mut reseed_snapshots = 0usize;
            for ev in &events {
                let out = dec.push(ev);
                for c in &out {
                    commits.push((
                        c.glyph,
                        c.sample_ts,
                        c.confidence.to_bits(),
                        c.alternatives
                            .iter()
                            .map(|(g, s)| (*g, s.to_bits()))
                            .collect::<Vec<_>>(),
                    ));
                }
                assert!(
                    dec.live.len() <= dec.cfg.beam + dec.cfg.seed_units_hops.len(),
                    "hop {}: snapshot bound beam + seed set exceeded: {}",
                    ev.hop,
                    dec.live.len()
                );
                assert!(
                    dec.scored_live_len <= dec.cfg.beam,
                    "hop {}: scored prefix exceeds the beam: {}",
                    ev.hop,
                    dec.scored_live_len
                );
                assert!(
                    dec.anchors.len() < 600,
                    "hop {}: anchors must expire, not accumulate: {}",
                    ev.hop,
                    dec.anchors.len()
                );
                if ev.hop == 217 + 117 * 11 + 8 {
                    // The previous push (the last cycle's gap anchor)
                    // reseeded, so live holds the u=13 seed and this push
                    // pruned with reach_sil = 80*13 = 1040: every anchor
                    // from the early cycles must be gone.
                    assert!(
                        dec.anchors.front().map(|a| a.hop) > Some(472),
                        "old anchors must expire: front is {:?}",
                        dec.anchors.front().map(|a| a.hop)
                    );
                }
                let raw: Vec<f32> = dec.live[dec.scored_live_len..]
                    .iter()
                    .map(|t| t.u)
                    .collect();
                if raw == vec![9.0, 13.0] {
                    reseed_snapshots += 1;
                }
            }
            (commits, reseed_snapshots)
        };
        let events = build_events();
        let span = events.last().unwrap().hop - events.first().unwrap().hop;
        assert!(
            span > (80.0 * 56.0) as u64,
            "the run must exceed 80*cfg.u_max, span {span}"
        );
        let (commits, reseed_snapshots) = run(events.clone());
        // Every one of the twelve cycles committed a Silence boundary and
        // reseeded. (The tail may add more reseed snapshots through junk
        // Silence commits, which the no-cooldown rule deliberately allows.)
        assert!(
            reseed_snapshots >= 12,
            "every distinct committed Silence must reseed; saw {reseed_snapshots}"
        );
        let boundary_ts: Vec<u64> = commits
            .iter()
            .filter(|(g, _, _, _)| g.is_none())
            .map(|(_, ts, _, _)| *ts)
            .collect();
        for k in 0..12u64 {
            assert!(
                boundary_ts.contains(&((120 + 117 * k) * 256)),
                "cycle {k} must commit its boundary at the mark anchor's ts"
            );
        }
        // Determinism: replaying the identical sequence reproduces the
        // serialized commit stream exactly.
        let (commits2, reseed_snapshots2) = run(events);
        assert_eq!(commits, commits2, "replay must be byte-identical");
        assert_eq!(reseed_snapshots, reseed_snapshots2);
    }
}
