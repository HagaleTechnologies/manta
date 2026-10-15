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
<<<<<<< HEAD
    /// MAN-189: input sample counter at the first hop of this entry's
    /// character -- the anchor its first mark began at. A word boundary
    /// carries the character it follows. `commit::Frontier` compares it
    /// with what has already been emitted.
    pub char_start_ts: u64,
=======
    /// Segment that closed this history entry. `Some(CGap)` on a
    /// glyph-carrying entry, `Some(WGap)`/`Some(Silence)` on both entries
    /// those transitions append, `None` for an entry with no closing
    /// segment (a synthetic unclosed entry, or a test fixture). Metadata
    /// only: it never feeds `token_order`, `merge_key`, confidence,
    /// consensus, or committed-history deduplication. MAN-192, SPEC v2
    /// §4.7 -- a committed word boundary whose closing segment was
    /// `Silence` is the decoder's second reseeding trigger.
    pub closing_segment: Option<SegType>,
>>>>>>> 3805f78bcb1b166f1eba98448d62db98addb201c
}

#[derive(Clone, Debug)]
pub struct Token {
    pub node: NodeId,
    pub phase: Phase,
    pub u: f32,
    pub score: f32,
    pub hist: Vec<HistEntry>,
    pub anchor_hop: u64,
    /// MAN-189: where the character this token is building began (the
    /// `seg_start_ts` of its first mark). Stale while the token is at
    /// `ROOT` between characters.
    pub char_start_ts: u64,
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
    /// evidence `evidence` (= `C[t] − C[s]`; negated internally for spaces).
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
        let char_start_ts = if seg.is_mark() && self.node == MorseTree::ROOT {
            seg_start_ts
        } else {
            self.char_start_ts
        };
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
<<<<<<< HEAD
                    char_start_ts,
=======
                    closing_segment: Some(seg),
>>>>>>> 3805f78bcb1b166f1eba98448d62db98addb201c
                });
                if seg != SegType::CGap {
                    hist.push(HistEntry {
                        glyph: None,
                        sample_ts: seg_start_ts,
                        born_hop: end_hop,
<<<<<<< HEAD
                        char_start_ts,
=======
                        closing_segment: Some(seg),
>>>>>>> 3805f78bcb1b166f1eba98448d62db98addb201c
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
            char_start_ts,
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
            char_start_ts: 0,
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
    fn hist_entry_carries_its_characters_first_mark_start() {
        // MAN-189: both entries a WGap closes (the glyph and the word
        // boundary) record where the character's first mark began, not
        // where its closing gap began; the next character's first mark
        // starts a fresh value.
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        let t = root_token(13.0)
            .successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 1000)
            .unwrap();
        let t = t
            .successor(SegType::EGap, 13, 50.0, tree, &cfg, 26, 2000)
            .unwrap();
        let t = t
            .successor(SegType::Dah, 39, 150.0, tree, &cfg, 65, 3000)
            .unwrap();
        let t = t
            .successor(SegType::WGap, 91, 300.0, tree, &cfg, 156, 4000)
            .unwrap();
        assert_eq!(t.hist.len(), 2);
        for e in &t.hist {
            assert_eq!((e.sample_ts, e.char_start_ts), (4000, 1000), "{e:?}");
        }
        let t = t
            .successor(SegType::Dah, 39, 150.0, tree, &cfg, 195, 5000)
            .unwrap();
        assert_eq!(t.char_start_ts, 5000);
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

    /// [Task 7 fix, round 2]: the round-1 correction of
    /// `speed_updates_only_on_short_segments` fixed a factually-wrong
    /// "glyphless node" premise, but its replacement exercises the same
    /// glyph-bearing node-T case the adjacent assertion already covers --
    /// leaving `Token::successor`'s `tree.glyph(self.node)?` early-return
    /// (the branch that actually rejects a gap from a glyphless node)
    /// untested. `"-.-.-"` (Dah, Dit, Dah, Dit, Dah) is a genuine glyphless
    /// interior node: it is the shared prefix of `;` (`"-.-.-."`) and `!`
    /// (`"-.-.--"`), so it has two children but, per `tree::TABLE`, no glyph
    /// entry of its own (verified: `TABLE` has no `"-.-.-"` entry). Reaching
    /// it and attempting a gap while still `AfterMark` (so the phase
    /// precondition is satisfied and the duration prior is in-range) isolates
    /// the glyph check specifically.
    #[test]
    fn gap_segment_from_glyphless_interior_node_is_invalid() {
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        // Walk "-.-.-" one element at a time (Dah, Dit, Dah, Dit, Dah), an
        // EGap of nominal length after each of the first four marks to
        // return to AfterSpace before the next mark; the final Dah is left
        // un-gapped so the token stays AfterMark on the glyphless node.
        let mut t = root_token(13.0);
        let mut hop = 0u64;
        for seg in [SegType::Dah, SegType::Dit, SegType::Dah, SegType::Dit] {
            let d = if seg == SegType::Dah { 39 } else { 13 }; // nominal at u=13
            hop += d as u64;
            t = t.successor(seg, d, 50.0, tree, &cfg, hop, 0).unwrap();
            hop += 13; // nominal EGap
            t = t
                .successor(SegType::EGap, 13, 50.0, tree, &cfg, hop, 0)
                .unwrap();
        }
        hop += 39;
        let t = t
            .successor(SegType::Dah, 39, 50.0, tree, &cfg, hop, 0)
            .unwrap();
        assert_eq!(t.phase, Phase::AfterMark);
        assert!(
            tree.glyph(t.node).is_none(),
            "\"-.-.-\" must be glyphless (shared prefix of ';' and '!')"
        );
        assert!(
            tree.child(t.node, Element::Dit).is_some()
                && tree.child(t.node, Element::Dah).is_some(),
            "\"-.-.-\" must have both children (';' and '!')"
        );
        // Phase precondition and duration prior both pass; only the glyph
        // check should reject each of CGap/WGap/Silence here.
        for seg in [SegType::CGap, SegType::WGap, SegType::Silence] {
            let d = match seg {
                SegType::CGap => 39,     // nominal 3*13
                SegType::WGap => 91,     // nominal 7*13
                SegType::Silence => 200, // within [10u, 80u]
                _ => unreachable!(),
            };
            assert!(
                t.successor(seg, d, 50.0, tree, &cfg, hop + d as u64, 0)
                    .is_none(),
                "{seg:?} from a glyphless node must be invalid"
            );
        }
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

    /// MAN-192: every real gap transition stamps the closing segment on
    /// every `HistEntry` it creates, so `commit` can tell a word boundary
    /// closed by a long `Silence` (the §4.7 reseed trigger) from one closed
    /// by an ordinary `WGap`.
    #[test]
    fn man192_gap_transitions_stamp_closing_segment() {
        let tree = MorseTree::shared();
        let cfg = HsmmConfig::default();
        // A single nominal Dah at u=13 lands on node T (glyph 'T') in
        // AfterMark, ready to be closed by each gap type.
        let t_node = |u: f32| root_token(u).successor(SegType::Dah, 39, 150.0, tree, &cfg, 39, 512);
        for (seg, d) in [
            (SegType::CGap, 39u32),  // nominal 3u
            (SegType::WGap, 91),     // nominal 7u
            (SegType::Silence, 200), // inside [10u, 80u] = [130, 1040]
        ] {
            let t = t_node(13.0).unwrap();
            let closed = t
                .successor(seg, d, 0.0, tree, &cfg, 39 + d as u64, 512)
                .unwrap_or_else(|| panic!("{seg:?} close of node T must be valid"));
            assert_eq!(closed.node, MorseTree::ROOT);
            assert_eq!(closed.phase, Phase::AfterSpace);
            // Nominal CGap/WGap durations and any in-window Silence leave
            // u at 13 (CGap updates speed but at d = 3u the update is a
            // no-op; WGap/Silence never update it).
            assert!(
                (closed.u - 13.0).abs() < 1e-5,
                "{seg:?}: u drifted to {}",
                closed.u
            );
            let glyph_entry = closed.hist[0];
            assert_eq!(glyph_entry.glyph, Some(Glyph::Char('T')), "{seg:?}");
            assert_eq!(glyph_entry.sample_ts, 512, "{seg:?}: pinned decision-11 ts");
            assert_eq!(glyph_entry.born_hop, 39 + d as u64, "{seg:?}");
            assert_eq!(glyph_entry.closing_segment, Some(seg), "{seg:?}");
            if seg == SegType::CGap {
                assert_eq!(closed.hist.len(), 1, "CGap appends no boundary");
            } else {
                assert_eq!(closed.hist.len(), 2, "{seg:?} appends glyph + boundary");
                let boundary = closed.hist[1];
                assert_eq!(boundary.glyph, None, "{seg:?}");
                assert_eq!(boundary.sample_ts, 512, "{seg:?}");
                assert_eq!(boundary.closing_segment, Some(seg), "{seg:?}");
            }
            // Inherited metadata survives further successors: a mark taken
            // after the close keeps the closed entries byte-identical.
            let resumed = closed
                .successor(SegType::Dit, 13, 50.0, tree, &cfg, 39 + d as u64 + 13, 600)
                .unwrap();
            assert_eq!(resumed.hist.len(), closed.hist.len(), "{seg:?}");
            for (old, new) in closed.hist.iter().zip(resumed.hist.iter()) {
                assert_eq!(old, new, "{seg:?}: inherited entry changed");
            }
        }
    }
}
