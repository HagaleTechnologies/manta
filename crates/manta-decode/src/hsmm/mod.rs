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
            hop: 0,
            last_prefix: 0.0,
            last_ts: 0,
            sealed: Vec::new(),
        }
    }

    /// The best live token's tracked speed (hops/dit), for `Evidence::set_u_ref`
    /// and WPM reporting. `None` before the first anchor -- and also, per
    /// [Task 8 fix, review round 2], before *any* live token has processed
    /// real evidence. `self.live` is reset to a completely fresh, unscored
    /// seed set (`score == 0.0`, `hist` empty) on every keying re-onset
    /// (`push`'s `present && !self.present` branch), and stays that way for
    /// however many hops it takes before `d` (the gap between an anchor and
    /// `ev.hop`) grows large enough for any seed's own duration prior to
    /// validate a candidate. `self.live` is sorted best-first once it holds
    /// real candidates, but immediately after a reseed it is *unsorted* raw
    /// seeds -- `live.first()` would then just be whichever seed happened
    /// to be pushed first (`cfg.seed_units_hops[0]`, 9.0 hops = 50 WPM),
    /// with zero evidence behind it. Reported directly to `set_u_ref`/
    /// `report_wpm` every such hop, this produced a spurious `SpeedUpdate`
    /// at an implausible WPM on every real inter-word or inter-transmission
    /// gap. Skipping any token that still looks exactly like a fresh seed
    /// closes this: a real token's score is the sum of at least one
    /// segment's evidence/duration/type-prior terms, which is practically
    /// never exactly `0.0`.
    pub fn best_u(&self) -> Option<f32> {
        self.live
            .iter()
            .find(|t| !(t.score == 0.0 && t.hist.is_empty()))
            .map(|t| t.u)
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
            if offset != 0.0 {
                for a in self.anchors.iter_mut() {
                    for t in a.tokens.iter_mut() {
                        t.score -= offset;
                    }
                }
            }
            let seeds = self.seed(ev.hop);
            self.anchors.push_back(Anchor {
                hop: ev.hop,
                sample_ts: ev.sample_ts,
                prefix: ev.prefix,
                min_hist_ts: min_hist_ts(&seeds),
                tokens: seeds.clone(),
            });
            self.live = seeds;
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
        // Commit consensus / forced entries.
        out.extend(commit::commit(
            &mut merged,
            ev.hop,
            &self.cfg,
            &mut self.sealed,
        ));
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
        if self.live.is_empty() {
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
                },
                token::HistEntry {
                    glyph: Some(Glyph::Char('B')),
                    sample_ts: 200,
                    born_hop: 10,
                },
            ],
            anchor_hop: 10,
        }];
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
            },
            token::HistEntry {
                glyph: Some(Glyph::Char('B')),
                sample_ts: 200,
                born_hop: 2,
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
                }],
                anchor_hop: 1,
            },
        ];
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
}
