//! Tokens: hypotheses over (tree node, phase, speed) with committed-glyph
//! history. SPEC v2 §4.5, §6.2.
use super::segments::SegType;
use super::HsmmConfig;
use crate::tree::{Element, Glyph, MorseTree, NodeId, Prosign};
use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Phase {
    AfterSpace,
    AfterMark,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistEntry {
    pub glyph: Option<Glyph>,
    pub sample_ts: u64,
    pub born_hop: u64,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub node: NodeId,
    pub phase: Phase,
    pub u: f32,
    pub score: f32,
    pub hist: Vec<HistEntry>,
    pub anchor_hop: u64,
}

pub fn glyph_rank(g: Option<Glyph>) -> u32 {
    match g {
        None => 0,
        Some(Glyph::Prosign(p)) => match p {
            Prosign::Ar => 1,
            Prosign::Sk => 2,
            Prosign::As => 3,
            Prosign::Sn => 4,
            Prosign::Err => 5,
        },
        Some(Glyph::Char(c)) => 16 + c as u32,
    }
}

/// SPEC v2 §6.2: score desc, hist lexical, u asc, node asc, phase.
pub fn token_order(a: &Token, b: &Token) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| {
            a.hist
                .iter()
                .map(|e| glyph_rank(e.glyph))
                .cmp(b.hist.iter().map(|e| glyph_rank(e.glyph)))
        })
        .then_with(|| a.u.total_cmp(&b.u))
        .then_with(|| a.node.cmp(&b.node))
        .then_with(|| a.phase.cmp(&b.phase))
}

pub fn merge_key(t: &Token) -> (NodeId, Phase, i32) {
    (t.node, t.phase, (2.0 * t.u).round() as i32)
}

impl Token {
    /// The token reached by ending segment `seg` of `d` hops with mark
    /// evidence `evidence` (= C[t] − C[s]; negated internally for spaces).
    /// `seg_start_ts` is the input sample counter at the segment's first hop
    /// (the timestamp a glyph committed by this gap carries, pinned decision 11).
    #[allow(clippy::too_many_arguments)]
    pub fn successor(
        &self,
        seg: SegType,
        d: u32,
        evidence: f32,
        tree: &MorseTree,
        cfg: &HsmmConfig,
        end_hop: u64,
        seg_start_ts: u64,
    ) -> Option<Token> {
        if self.phase != seg.ends_phase() {
            return None;
        }
        let lp_dur = seg.log_dur_prior(d, self.u, cfg)?;
        let ev = if seg.is_mark() { evidence } else { -evidence };
        let score = self.score + ev + lp_dur + seg.log_type_prior(cfg);
        let mut u = self.u;
        if seg.updates_speed() {
            u += cfg.speed_alpha * (d as f32 / seg.k() - u);
            u = u.clamp(cfg.u_min, cfg.u_max);
        }
        let mut hist = self.hist.clone();
        let (node, phase) = match seg {
            SegType::Dit | SegType::Dah => {
                let e = if seg == SegType::Dit {
                    Element::Dit
                } else {
                    Element::Dah
                };
                (tree.child(self.node, e)?, Phase::AfterMark)
            }
            SegType::EGap => (self.node, Phase::AfterSpace),
            SegType::CGap | SegType::WGap | SegType::Silence => {
                let g = tree.glyph(self.node)?;
                hist.push(HistEntry {
                    glyph: Some(g),
                    sample_ts: seg_start_ts,
                    born_hop: end_hop,
                });
                if seg != SegType::CGap {
                    hist.push(HistEntry {
                        glyph: None,
                        sample_ts: seg_start_ts,
                        born_hop: end_hop,
                    });
                }
                (MorseTree::ROOT, Phase::AfterSpace)
            }
        };
        Some(Token {
            node,
            phase,
            u,
            score,
            hist,
            anchor_hop: end_hop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Element, MorseTree};

    fn root_token(u: f32) -> Token {
        Token {
            node: MorseTree::ROOT,
            phase: Phase::AfterSpace,
            u,
            score: 0.0,
            hist: Vec::new(),
            anchor_hop: 0,
        }
    }

    #[test]
    fn dit_then_dah_then_cgap_emits_a() {
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        let t = root_token(13.0);
        let t = t
            .successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0)
            .unwrap();
        assert_eq!(t.phase, Phase::AfterMark);
        assert_eq!(t.node, tree.child(MorseTree::ROOT, Element::Dit).unwrap());
        let t = t
            .successor(SegType::EGap, 13, 50.0, tree, &cfg, 26, 13 * 512)
            .unwrap();
        let t = t
            .successor(SegType::Dah, 39, 150.0, tree, &cfg, 65, 26 * 512)
            .unwrap();
        let t = t
            .successor(SegType::CGap, 39, 150.0, tree, &cfg, 104, 65 * 512)
            .unwrap();
        assert_eq!(t.node, MorseTree::ROOT);
        assert_eq!(t.hist.len(), 1);
        assert_eq!(t.hist[0].glyph, Some(Glyph::Char('A')));
        assert_eq!(t.hist[0].sample_ts, 65 * 512);
        assert!((t.u - 13.0).abs() < 1e-4, "u drifted to {}", t.u);
    }

    #[test]
    fn invalid_transitions_return_none() {
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        let t = root_token(13.0);
        assert!(t
            .successor(SegType::EGap, 13, 0.0, tree, &cfg, 13, 0)
            .is_none()); // space from AfterSpace
        assert!(t
            .successor(SegType::CGap, 39, 0.0, tree, &cfg, 39, 0)
            .is_none());
        let t = t
            .successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0)
            .unwrap();
        assert!(t
            .successor(SegType::Dit, 13, 50.0, tree, &cfg, 26, 0)
            .is_none()); // mark from AfterMark
                         // 8 dits deep: fall off the tree
        let mut t = root_token(13.0);
        for i in 0..8 {
            let m = t.successor(SegType::Dit, 13, 50.0, tree, &cfg, 26 * i + 13, 0);
            match m {
                Some(m) => {
                    t = m
                        .successor(SegType::EGap, 13, 50.0, tree, &cfg, 26 * i + 26, 0)
                        .unwrap()
                }
                None => {
                    assert!(i >= 5, "fell off at depth {i}");
                    return;
                }
            }
        }
        panic!("never fell off the tree");
    }

    #[test]
    fn speed_updates_only_on_short_segments() {
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        let t = root_token(10.0)
            .successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0)
            .unwrap();
        assert!((t.u - (10.0 + 0.2 * (13.0 - 10.0))).abs() < 1e-5);
        // [Task 7 fix]: the brief's comment ("WGap from a glyphless node must
        // be invalid") is factually wrong for this node -- `tree::TABLE`
        // maps the single-dit pattern "." directly to Glyph::Char('E'), so
        // `t.node` here *is* glyph-bearing (verified against
        // crates/manta-decode/src/tree.rs's TABLE and MorseTree::build).
        // Per SPEC v2 §4.2, CGap/WGap/Silence are allowed from any
        // glyph-bearing node, and the observed duration (d=91 against
        // nom=7*10.6=74.2, range [44.52, 111.3]) is in-range, so this
        // transition is legitimately valid: it commits 'E' plus a word
        // boundary. Corrected to assert the valid-transition behavior,
        // matching the pattern the node-T case below already exercises.
        let t = t
            .successor(SegType::WGap, 91, 300.0, tree, &cfg, 104, 0)
            .unwrap();
        assert!(
            (t.u - (10.0 + 0.2 * (13.0 - 10.0))).abs() < 1e-5,
            "WGap must not update u"
        );
        assert_eq!(t.hist.len(), 2);
        assert_eq!(t.hist[0].glyph, Some(Glyph::Char('E')));
        assert_eq!(t.hist[1].glyph, None);
        let e = root_token(13.0)
            .successor(SegType::Dah, 39, 100.0, tree, &cfg, 39, 0)
            .unwrap(); // node T
        let w = e
            .successor(SegType::WGap, 100, 100.0, tree, &cfg, 139, 0)
            .unwrap();
        assert!((w.u - e.u).abs() < 1e-6, "WGap must not update u");
        assert_eq!(w.hist.len(), 2); // glyph T, then Word
        assert_eq!(w.hist[1].glyph, None);
    }

    #[test]
    fn ordering_is_total_and_matches_spec() {
        let mut a = root_token(13.0);
        a.score = 1.0;
        let mut b = root_token(13.0);
        b.score = 2.0;
        assert_eq!(token_order(&b, &a), std::cmp::Ordering::Less); // higher score first
        let mut c = root_token(12.0);
        c.score = 1.0;
        assert_eq!(token_order(&c, &a), std::cmp::Ordering::Less); // equal score: smaller u first
        assert_eq!(merge_key(&a), (MorseTree::ROOT, Phase::AfterSpace, 26));
    }
}
