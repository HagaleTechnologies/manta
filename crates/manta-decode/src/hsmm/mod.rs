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
    /// [Task 8 fix] `(sample_ts, glyph)` pairs ever actually committed; see
    /// `commit::commit`'s doc comment for why this must be keyed on
    /// `sample_ts` (not `born_hop`) and threaded through every call.
    sealed: Vec<(u64, Option<Glyph>)>,
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
            // Keying just appeared: (re)seed at this hop.
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
        self.anchors.push_back(Anchor {
            hop: ev.hop,
            sample_ts: ev.sample_ts,
            prefix: ev.prefix,
            min_hist_ts: min_hist_ts(&merged),
            tokens: merged,
        });
        while let Some(front) = self.anchors.front() {
            if ev.hop - front.hop > reach_sil {
                self.anchors.pop_front();
            } else {
                break;
            }
        }
        // [Task 8 fix, review round 3] Bound `sealed`'s growth: the safe
        // prune bound is the minimum `sample_ts` over EVERY hist entry of
        // EVERY token in EVERY currently-live anchor -- not that anchor's
        // own `sample_ts` (round 2's bug). `Token::successor` clones an
        // ancestor's whole `hist` and only stamps the *new* entry with the
        // current anchor's own timestamp; an inherited older entry keeps
        // whatever timestamp it was originally stamped with, from a
        // possibly-since-evicted ancestor anchor. So a still-live anchor's
        // frozen tokens can carry a `hist` entry older than that anchor's
        // OWN `sample_ts` (and older than the oldest anchor's own
        // `sample_ts`, if that anchor is a newer one that inherited an old
        // entry) -- pruning `sealed` by anchor timestamp alone could drop
        // an entry a still-live anchor can still regenerate, reopening
        // exactly the duplicate-commit failure this list exists to
        // prevent. `Anchor::min_hist_ts` is computed once at anchor
        // creation (O(1) amortized here: this is just a min-of-mins over
        // already-live anchors, not a fresh per-token/per-hist rescan).
        match self.anchors.iter().filter_map(|a| a.min_hist_ts).min() {
            Some(bound) => self.sealed.retain(|&(ts, _)| ts >= bound),
            None => self.sealed.clear(),
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
        for t in self.live.iter_mut() {
            while t
                .hist
                .first()
                .is_some_and(|e| self.sealed.contains(&(e.sample_ts, e.glyph)))
            {
                t.hist.remove(0);
            }
        }
        self.live.sort_by(token_order);
        let best = self.live[0].clone();
        let hist = best.hist.clone();
        for (j, e) in hist.iter().enumerate() {
            let (confidence, alternatives) = commit::margin(&self.live, j, &self.cfg);
            out.push(Committed {
                glyph: e.glyph,
                sample_ts: e.sample_ts,
                confidence,
                alternatives,
            });
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
}
