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
    /// and WPM reporting. `None` before the first anchor.
    pub fn best_u(&self) -> Option<f32> {
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
            tokens: merged,
        });
        while let Some(front) = self.anchors.front() {
            if ev.hop - front.hop > reach_sil {
                self.anchors.pop_front();
            } else {
                break;
            }
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
