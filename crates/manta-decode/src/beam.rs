//! Character-local beam search over the Morse tree. SPEC §4.3–§4.5, §10.3.

use crate::timing::ClassEstimate;
use crate::tree::{Element, Glyph, MorseTree, NodeId, Prosign};

/// Tunables for the character-local beam search. SPEC §9, §4.4.
#[derive(Debug, Clone)]
pub struct BeamConfig {
    /// SPEC §9 decode.beam_width
    pub width: usize,
    /// SPEC §9 decode.timing_sigma — "the riskiest constant in the spec"
    pub sigma: f32,
}

impl Default for BeamConfig {
    fn default() -> Self {
        BeamConfig {
            width: 4,
            sigma: 0.25,
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
    decode_char_readings(marks_ms, mu_dit_ms, mu_dah_ms, None, None, q, cfg).map(|readings| {
        let winner = readings[0];
        CharDecode {
            glyph: winner.glyph,
            confidence: winner.confidence,
        }
    })
}

/// One acoustically supported character with its unnormalized log likelihood.
#[derive(Debug, Clone, Copy)]
pub struct CharReading {
    pub glyph: Glyph,
    pub score: f32,
    pub confidence: f32,
}

/// Convert sender variance in dits to log-duration sigma by the delta method.
/// Sparse or invalid estimates keep the configured early-track sigma. The
/// finite floor prevents zero variance from producing an infinite likelihood.
pub fn class_sigma(est: &ClassEstimate, floor: u32, fallback: f32) -> f32 {
    if est.n < floor
        || !est.mean_dits.is_finite()
        || est.mean_dits <= 0.0
        || !est.var_dits2.is_finite()
        || est.var_dits2 < 0.0
    {
        return fallback;
    }
    (est.var_dits2.sqrt() / est.mean_dits).max(0.02)
}

/// Return every glyph-bearing survivor, ordered by acoustic likelihood.
/// `None` denotes garble; a single question mark denotes glyphless survivors.
/// When sender sigmas differ, retain their normalizers: omitting these would
/// reward a broad duration class merely for having a flatter likelihood.
/// The duration Jacobian and common Gaussian constant cancel between paths.
pub fn decode_char_readings(
    marks_ms: &[f32],
    mu_dit_ms: f32,
    mu_dah_ms: f32,
    dot_sigma: Option<f32>,
    dash_sigma: Option<f32>,
    q: f32,
    cfg: &BeamConfig,
) -> Option<Vec<CharReading>> {
    if marks_ms.is_empty() {
        return None;
    }
    // SPEC §4.4 error prosign: >= 6 dit-classified marks, no dah.
    let boundary = (mu_dit_ms * mu_dah_ms).sqrt();
    if marks_ms.len() >= 6 && marks_ms.iter().all(|&d| d < boundary) {
        return Some(vec![CharReading {
            glyph: Glyph::Prosign(Prosign::Err),
            score: 0.0,
            confidence: q,
        }]);
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
            for (e, mu, learned_sigma) in [
                (Element::Dit, mu_dit_ms, dot_sigma),
                (Element::Dah, mu_dah_ms, dash_sigma),
            ] {
                let sigma = learned_sigma.unwrap_or(cfg.sigma);
                let normalizer = if dot_sigma.is_some() || dash_sigma.is_some() {
                    (sigma / cfg.sigma).ln()
                } else {
                    0.0
                };
                if let Some(child) = tree.child(h.node, e) {
                    let mut path = h.path.clone();
                    path.push(e);
                    next.push(Hyp {
                        node: child,
                        score: h.score + log_likelihood(d, mu, sigma) - normalizer,
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
        next.truncate(cfg.width);
        hyps = next;
    }

    // Boundary: drop glyphless survivors (they stay sorted).
    let survivors: Vec<&Hyp> = hyps
        .iter()
        .filter(|h| tree.glyph(h.node).is_some())
        .collect();
    if survivors.is_empty() {
        return Some(vec![CharReading {
            glyph: Glyph::Char('?'),
            score: 0.0,
            confidence: 0.0,
        }]);
    }
    // SPEC §4.5 softmax with max-subtraction, fixed order; winner is survivors[0].
    let smax = survivors[0].score;
    let mut denom = 0.0f32;
    for h in &survivors {
        denom += (h.score - smax).exp();
    }
    Some(
        survivors
            .into_iter()
            .map(|h| CharReading {
                glyph: tree.glyph(h.node).unwrap(),
                score: h.score,
                confidence: ((h.score - smax).exp() / denom) * q,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Glyph, Prosign};

    const CFG: BeamConfig = BeamConfig {
        width: 4,
        sigma: 0.25,
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

    #[test]
    fn ambiguous_third_mark_keeps_w_and_r_readings_ranked() {
        let readings =
            decode_char_readings(&[60.0, 180.0, 103.0], 60.0, 180.0, None, None, 1.0, &CFG)
                .unwrap();
        let w = readings
            .iter()
            .find(|r| r.glyph == Glyph::Char('W'))
            .unwrap();
        let r = readings
            .iter()
            .find(|r| r.glyph == Glyph::Char('R'))
            .unwrap();
        assert!(r.score > w.score);
        assert!(r.score - w.score < 1.0);
        assert!(readings.windows(2).all(|r| r[0].score >= r[1].score));
    }

    #[test]
    fn per_sender_sigma_rescores_candidates() {
        let marks = [60.0, 180.0, 103.0];
        let broad =
            decode_char_readings(&marks, 60.0, 180.0, Some(0.5), Some(0.5), 1.0, &CFG).unwrap();
        let narrow =
            decode_char_readings(&marks, 60.0, 180.0, Some(0.5), Some(0.05), 1.0, &CFG).unwrap();
        let gap = |readings: &[CharReading]| {
            let r = readings
                .iter()
                .find(|r| r.glyph == Glyph::Char('R'))
                .unwrap()
                .score;
            let w = readings
                .iter()
                .find(|r| r.glyph == Glyph::Char('W'))
                .unwrap()
                .score;
            r - w
        };
        assert!(gap(&broad).abs() < 1.0);
        assert!(gap(&narrow) > 20.0);
    }

    #[test]
    fn sparse_class_falls_back_to_configured_sigma() {
        let estimate = crate::timing::ClassEstimate {
            mean_dits: 3.0,
            var_dits2: 0.09,
            n: 7,
        };
        assert_eq!(class_sigma(&estimate, 8, CFG.sigma), CFG.sigma);
        let learned = crate::timing::ClassEstimate { n: 8, ..estimate };
        assert!((class_sigma(&learned, 8, CFG.sigma) - 0.1).abs() < 1e-6);
        let zero = crate::timing::ClassEstimate {
            var_dits2: 0.0,
            ..learned
        };
        assert!(class_sigma(&zero, 8, CFG.sigma) > 0.0);
    }

    #[test]
    fn class_sigma_matches_direct_log_space_ew_estimate() {
        // The delta method is a local approximation. Compare it against the
        // same EW update in log space for modest sender jitter.
        let alpha = 0.1;
        let mut mean = 0.0f32;
        let mut variance = 0.0f32;
        let mut log_mean = 0.0f32;
        let mut log_variance = 0.0f32;
        for i in 0..200 {
            let x = 3.0 + (i % 5) as f32 * 0.05 - 0.1;
            if i == 0 {
                mean = x;
                log_mean = x.ln();
            } else {
                let delta = x - mean;
                mean += alpha * delta;
                variance = (1.0 - alpha) * (variance + alpha * delta * delta);
                let log_delta = x.ln() - log_mean;
                log_mean += alpha * log_delta;
                log_variance = (1.0 - alpha) * (log_variance + alpha * log_delta * log_delta);
            }
        }
        let estimate = crate::timing::ClassEstimate {
            mean_dits: mean,
            var_dits2: variance,
            n: 200,
        };
        let delta_sigma = class_sigma(&estimate, 8, CFG.sigma);
        let direct_sigma = log_variance.sqrt();
        assert!((delta_sigma - direct_sigma).abs() / direct_sigma < 0.02);
    }

    #[test]
    fn readings_match_decode_char_winner_in_all_modes() {
        let one = BeamConfig { width: 1, ..CFG };
        let glyphless = decode_char_readings(
            &[60.0, 60.0, 180.0, 180.0],
            60.0,
            180.0,
            None,
            None,
            1.0,
            &one,
        )
        .unwrap();
        assert_eq!(glyphless[0].glyph, Glyph::Char('?'));
        assert_eq!(glyphless[0].confidence, 0.0);
        assert!(decode_char_readings(&[180.0; 8], 60.0, 180.0, None, None, 1.0, &CFG).is_none());
        let error = decode_char_readings(&[60.0; 8], 60.0, 180.0, None, None, 0.8, &CFG).unwrap();
        assert_eq!(error[0].glyph, Glyph::Prosign(Prosign::Err));
        assert_eq!(error[0].confidence, 0.8);
        for marks in [
            vec![],
            vec![60.0, 180.0],
            vec![100.0, 140.0, 70.0],
            vec![60.0; 8],
            vec![180.0; 8],
            vec![180.0; 6],
        ] {
            let old = decode_char(&marks, 60.0, 180.0, 0.8, &CFG);
            let readings = decode_char_readings(&marks, 60.0, 180.0, None, None, 0.8, &CFG);
            match (old, readings) {
                (None, None) => {}
                (Some(old), Some(readings)) => {
                    assert_eq!(old.glyph, readings[0].glyph);
                    assert_eq!(old.confidence.to_bits(), readings[0].confidence.to_bits());
                }
                _ => panic!("garble outcome differs"),
            }
        }
    }
}
