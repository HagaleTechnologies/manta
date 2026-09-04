//! Character-local beam search over the Morse tree. SPEC §4.3–§4.5, §10.3.

use crate::tree::{Element, Glyph, MorseTree, NodeId, Prosign};

/// Tunables for the character-local beam search. SPEC §9, §4.4.
#[derive(Debug, Clone)]
pub struct BeamConfig {
    /// SPEC §9 decode.beam_width
    pub width: usize,
    /// SPEC §9 decode.timing_sigma — "the riskiest constant in the spec"
    pub sigma: f32,
    /// **[DEVIATION from SPEC §9 decode.beam_width]** MAN-9: beam width
    /// used instead of `width` when the per-character channel-quality term
    /// `q` (SPEC §4.5) is below `q_low`. Under fading, distorted mark
    /// durations push the correct dit/dah hypothesis out of the top
    /// `width` survivors at an intermediate truncation step, after which
    /// it can never recover (the beam is character-local and greedy
    /// across characters, SPEC §4.3-4.5) -- widening only for low-quality
    /// characters buys that back without paying the extra `O(width·marks)`
    /// cost on every clean character (Pi 4 CPU budget, ROADMAP M2).
    /// Defaults to `width` (inert = SPEC behavior). See
    /// docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md.
    pub width_low_q: usize,
    /// Threshold on `q`, exclusive-below (`q < q_low` selects
    /// `width_low_q`). SPEC §4.5 clamps `q` to `[0.3, 1.0]`.
    pub q_low: f32,
}

impl Default for BeamConfig {
    fn default() -> Self {
        let width = 4;
        BeamConfig {
            width,
            sigma: 0.25,
            width_low_q: width, // inert: SPEC behavior, see the field's doc comment
            q_low: 0.6,
        }
    }
}

impl BeamConfig {
    /// The beam width to use for a character decoded at channel quality
    /// `q`. MAN-9: `width_low_q` below `q_low`, `width` otherwise.
    pub fn effective_width(&self, q: f32) -> usize {
        if q < self.q_low {
            self.width_low_q
        } else {
            self.width
        }
    }
}

/// One decoded character: the winning glyph and its confidence. SPEC §4.4–§4.5.
#[derive(Debug, Clone, Copy)]
pub struct CharDecode {
    /// The highest-score glyph-bearing survivor.
    pub glyph: Glyph,
    /// Softmax-derived confidence, already scaled by channel quality `q`. SPEC §4.5.
    pub confidence: f32,
}

/// Log-normal mark likelihood. SPEC §4.3.
pub fn log_likelihood(dur_ms: f32, mu_ms: f32, sigma: f32) -> f32 {
    let d = dur_ms.ln() - mu_ms.ln();
    -(d * d) / (2.0 * sigma * sigma)
}

#[derive(Debug, Clone)]
struct Hyp {
    node: NodeId,
    score: f32,
    path: Vec<Element>,
}

/// Decode one character from its mark durations. Character-local: the beam
/// resets at every character boundary (SPEC §10.3); the caller owns boundary
/// detection (SPEC §4.2 gap classification).
///
/// Returns:
/// - `None`: aborted garble — every branch fell off the tree (SPEC §4.4.2),
///   or no marks. Emits nothing; caller counts it as a decode error.
/// - `Some(Glyph::Char('?'), 0.0)`: survivors exist but none carries a glyph
///   (SPEC §4.4.4).
/// - `Some(Glyph::Prosign(Err), ..)`: operator error prosign (SPEC §4.4).
pub fn decode_char(
    marks_ms: &[f32],
    mu_dit_ms: f32,
    mu_dah_ms: f32,
    q: f32,
    cfg: &BeamConfig,
) -> Option<CharDecode> {
    if marks_ms.is_empty() {
        return None;
    }
    // SPEC §4.4 error prosign: >= 6 dit-classified marks, no dah.
    let boundary = (mu_dit_ms * mu_dah_ms).sqrt();
    if marks_ms.len() >= 6 && marks_ms.iter().all(|&d| d < boundary) {
        return Some(CharDecode {
            glyph: Glyph::Prosign(Prosign::Err),
            confidence: q,
        });
    }

    let tree = MorseTree::shared();
    let mut hyps = vec![Hyp {
        node: MorseTree::ROOT,
        score: 0.0,
        path: Vec::new(),
    }];
    for &d in marks_ms {
        let mut next: Vec<Hyp> = Vec::with_capacity(hyps.len() * 2);
        for h in &hyps {
            for (e, mu) in [(Element::Dit, mu_dit_ms), (Element::Dah, mu_dah_ms)] {
                if let Some(child) = tree.child(h.node, e) {
                    let mut path = h.path.clone();
                    path.push(e);
                    next.push(Hyp {
                        node: child,
                        score: h.score + log_likelihood(d, mu, cfg.sigma),
                        path,
                    });
                }
            }
        }
        if next.is_empty() {
            return None; // all branches dropped: garble, emits nothing
        }
        // SPEC §6.5: deterministic order — score desc, then path lex (dit < dah).
        next.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        next.truncate(cfg.effective_width(q));
        hyps = next;
    }

    // Boundary: drop glyphless survivors (they stay sorted).
    let survivors: Vec<&Hyp> = hyps
        .iter()
        .filter(|h| tree.glyph(h.node).is_some())
        .collect();
    if survivors.is_empty() {
        return Some(CharDecode {
            glyph: Glyph::Char('?'),
            confidence: 0.0,
        });
    }
    // SPEC §4.5 softmax with max-subtraction, fixed order; winner is survivors[0].
    let smax = survivors[0].score;
    let mut denom = 0.0f32;
    for h in &survivors {
        denom += (h.score - smax).exp();
    }
    let confidence = (1.0 / denom) * q;
    Some(CharDecode {
        glyph: tree.glyph(survivors[0].node).unwrap(),
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Glyph, Prosign};

    const CFG: BeamConfig = BeamConfig {
        width: 4,
        sigma: 0.25,
        width_low_q: 4,
        q_low: 0.6,
    };

    #[test]
    fn log_likelihood_is_zero_at_centroid_and_symmetric_in_log() {
        assert_eq!(log_likelihood(60.0, 60.0, 0.25), 0.0);
        // SPEC §4.3: ll = -(ln d - ln mu)^2 / (2 sigma^2)
        let l = log_likelihood(120.0, 60.0, 0.25);
        let expected = -(2.0f32.ln().powi(2)) / (2.0 * 0.25 * 0.25);
        assert!((l - expected).abs() < 1e-5);
        assert!((log_likelihood(30.0, 60.0, 0.25) - l).abs() < 1e-5);
    }

    /// A distorted mark sequence whose correct glyph is pruned at width 4
    /// must be recovered at width 12. '5' = `.....`; the marks below are a
    /// real found-by-search instance (element durations perturbed as a
    /// Watterson-style fade might) where the 3rd mark stretches into
    /// dah-ish territory (114.1 ms, above the 86.6 ms dit/dah boundary at
    /// mu_dit=50/mu_dah=150), pushing the true `.....` hypothesis out of
    /// the top 4 survivors by the time the beam reaches it -- width 4
    /// commits to '2' (`..---`) instead, which width 12 avoids.
    #[test]
    fn wider_beam_recovers_a_glyph_pruned_at_width_4() {
        let marks = [46.55, 83.05, 114.1, 86.25, 63.75];
        let narrow_cfg = BeamConfig {
            width: 4,
            width_low_q: 4,
            ..CFG
        };
        let wide_cfg = BeamConfig {
            width: 12,
            width_low_q: 12,
            ..CFG
        };
        let narrow = decode_char(&marks, 50.0, 150.0, 0.4, &narrow_cfg).unwrap();
        let wide = decode_char(&marks, 50.0, 150.0, 0.4, &wide_cfg).unwrap();
        assert_ne!(narrow.glyph, wide.glyph);
        assert_eq!(wide.glyph, Glyph::Char('5'));
    }

    /// Width selection must be deterministic and depend only on `q`.
    #[test]
    fn low_q_selects_the_wide_beam_and_high_q_does_not() {
        let cfg = BeamConfig {
            width: 4,
            width_low_q: 12,
            q_low: 0.6,
            sigma: 0.25,
        };
        assert_eq!(cfg.effective_width(0.95), 4);
        assert_eq!(cfg.effective_width(0.40), 12);
        assert_eq!(cfg.effective_width(0.60), 4); // boundary is exclusive-below
    }

    /// Defaults reproduce today exactly: `width_low_q` starts equal to
    /// `width`, so `effective_width` is constant regardless of `q`.
    #[test]
    fn beam_width_low_q_defaults_to_the_base_width() {
        let d = BeamConfig::default();
        assert_eq!(d.width_low_q, d.width);
    }

    #[test]
    fn clean_character_decodes_with_high_confidence() {
        // 'A' = .- at 20 WPM
        let r = decode_char(&[60.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Char('A'));
        assert!(r.confidence > 0.9, "confidence {}", r.confidence);
    }

    #[test]
    fn marginal_mark_keeps_both_hypotheses_alive() {
        // Mark at 100 ms is ambiguous between dit(60) and dah(180); the
        // second mark (clean dah) disambiguates via the tree: ".-" = A vs "--" = M.
        let r = decode_char(&[100.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        // Both A and M survive to the boundary; winner has confidence < 1.
        assert!(r.confidence < 0.999);
        assert!(matches!(r.glyph, Glyph::Char('A') | Glyph::Char('M')));
    }

    #[test]
    fn tie_breaks_dit_before_dah() {
        // Equal mu => identical scores for both branches. SPEC §6.5:
        // element-sequence lexical order, dit < dah => 'E' wins over 'T'.
        let r = decode_char(&[100.0], 100.0, 100.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Char('E'));
        assert!((r.confidence - 0.5).abs() < 1e-4); // two equal survivors
    }

    #[test]
    fn error_prosign_on_dit_run() {
        // SPEC §4.4: >= 6 dit-classified marks with no dah -> <ERR>.
        let marks = [60.0; 8];
        let r = decode_char(&marks, 60.0, 180.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Prosign(Prosign::Err));
    }

    #[test]
    fn too_long_sequence_aborts_as_garble() {
        // 8 dahs: every 8-element path falls off the tree (max depth 7) -> None.
        let marks = [180.0; 8];
        assert!(decode_char(&marks, 60.0, 180.0, 1.0, &CFG).is_none());
    }

    #[test]
    fn q_scales_confidence() {
        let hi = decode_char(&[60.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        let lo = decode_char(&[60.0, 180.0], 60.0, 180.0, 0.3, &CFG).unwrap();
        assert!((lo.confidence - 0.3 * hi.confidence).abs() < 1e-5);
    }

    #[test]
    fn empty_marks_is_none() {
        assert!(decode_char(&[], 60.0, 180.0, 1.0, &CFG).is_none());
    }

    #[test]
    fn deterministic_across_runs() {
        let marks = [100.0, 140.0, 70.0];
        let a = decode_char(&marks, 60.0, 180.0, 0.8, &CFG).unwrap();
        let b = decode_char(&marks, 60.0, 180.0, 0.8, &CFG).unwrap();
        assert_eq!(a.glyph, b.glyph);
        assert_eq!(a.confidence.to_bits(), b.confidence.to_bits());
    }
}
