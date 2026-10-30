//! MAN-106: the committed empirical confidence calibration. `c_call`
//! (below in `confidence`) is a raw composition whose scale was never
//! checked against accuracy; `CALIBRATION` turns it into a probability
//! that the decoded callsign is correct, fitted offline on deterministic
//! bench scenes with withheld validator candidates as hard negatives and
//! committed with reproducible provenance.
//!
//! Provenance: `scripts/fit-confidence-calibration.py` fits these
//! constants from the committed measurement pair
//! `data/calibration/{fit,heldout}.csv` (rows `c_call_raw,reps,tier,
//! correct` extracted from `manta bench sensitivity --json` decision
//! rows; the recipe and split rule live in the script). Regenerate with
//! `python3 scripts/fit-confidence-calibration.py --emit-rust`, which
//! reproduces this block byte-for-byte from the CSVs (the python test
//! `scripts.tests.test_fit_calibration` pins that). The committed
//! constants land in `data/calibration/constants.json` for the Rust-side
//! equality test. Fit and held-out quality figures are recorded in
//! docs/DECISIONS/2026-10-30-man106-confidence-calibration.md.

use crate::confidence::geo_mean;
use crate::PatternTier;

/// A monotone piecewise-linear calibration over the joint raw score
/// `geo_mean · (1 − 0.5^r)^rep_scale · tier_factor[tier]`, fitted jointly
/// (not stacked) on labeled bench data. `knots` is ascending in the raw
/// score and non-decreasing in the calibrated probability.
pub struct Calibration {
    /// `(joint_raw, probability)` pairs, `joint_raw` strictly ascending.
    /// The first knot anchors raw 0 to probability 0.
    pub knots: &'static [(f32, f32)],
    /// Multipliers per rarity tier, `[common, listed, unlisted]`
    /// (MAN-95). `1.0` is neutral.
    pub tier_factors: &'static [f32; 3],
    /// Exponent re-fitted on the repetition factor `1 − 0.5^r`. `1.0` is
    /// the raw composition's own scale.
    pub rep_scale: f32,
}

/// The committed calibration (MAN-106). Fitted by
/// `scripts/fit-confidence-calibration.py`; do not hand-edit. The map's
/// soft-step shape is real: emission is conditioned on the repetition
/// gate, so a candidate that clears its pattern tier's rep count with a
/// clean geometric mean is, in the labeled bench population, correct
/// with ~0.96 probability, while everything below joint ≈ 0.17 is
/// garble. See docs/DECISIONS/2026-10-30-man106-confidence-calibration.md.
pub const CALIBRATION: Calibration = Calibration {
    knots: &[
        (0.0, 0.0),
        (0.074991, 0.00375),
        (0.096076, 0.004804),
        (0.168725, 0.958436),
        (0.168747, 0.958437),
        (0.238496, 0.961925),
        (0.300565, 0.965028),
        (0.423574, 0.971179),
    ],
    tier_factors: &[1.0, 0.7, 0.5],
    rep_scale: 2.0,
};

/// A spot whose calibrated probability reaches this publishes as
/// `confirmed`; below it the spot still publishes, as `tentative`
/// (MAN-106 reviewer question 2's default: only the zero-evidence
/// abstention suppresses emission). Chosen from the Phase-3
/// risk-vs-coverage table at the point risk ≤ 5%; see the calibration
/// decision record.
pub const PUBLISH_CONFIRMED_THRESHOLD: f32 = 0.5;

fn tier_index(tier: PatternTier) -> usize {
    match tier {
        PatternTier::Common => 0,
        PatternTier::Listed => 1,
        PatternTier::Unlisted => 2,
    }
}

impl Calibration {
    /// The repetition factor at `rep_scale`: `(1 − 0.5^r)^rep_scale`.
    fn rep_factor(&self, reps: u32) -> f32 {
        (1.0 - 0.5f32.powi(reps.max(1) as i32)).powf(self.rep_scale)
    }

    /// The joint raw score a candidate's evidence maps through: the raw
    /// `c_call` composition's geo-mean, with the repetition factor and
    /// the pattern-tier rarity multiplier folded in at their fitted
    /// scales.
    pub fn joint_raw(&self, geo_mean: f32, reps: u32, tier: PatternTier) -> f32 {
        geo_mean * self.rep_factor(reps) * self.tier_factors[tier_index(tier)]
    }

    /// The calibrated probability for a candidate whose per-character
    /// confidences are `char_confidences`, decoded `reps` times, at
    /// pattern rarity `tier`.
    pub fn calibrated(&self, char_confidences: &[f32], reps: u32, tier: PatternTier) -> f32 {
        self.map(self.joint_raw(geo_mean(char_confidences), reps, tier))
    }

    /// The calibrated probability recovered from a decision's raw
    /// `c_call` value (`geo_mean · (1 − 0.5^r)`), the shape
    /// `Decision::confidence` records, so bench-side consumers can label
    /// withheld candidates without the character confidences.
    pub fn calibrated_from_raw(&self, c_call_raw: f32, reps: u32, tier: PatternTier) -> f32 {
        let base = 1.0 - 0.5f32.powi(reps.max(1) as i32);
        // c_call_raw = geo · base, so joint = c_call_raw · base^(s-1) · tf.
        let joint =
            c_call_raw * base.powf(self.rep_scale - 1.0) * self.tier_factors[tier_index(tier)];
        self.map(joint)
    }

    /// Piecewise-linear interpolation through `knots`, clamped at both
    /// ends. Monotone by construction (the knots are).
    pub fn map(&self, x: f32) -> f32 {
        let knots = self.knots;
        if x <= knots[0].0 {
            return knots[0].1;
        }
        for w in knots.windows(2) {
            let (x0, y0) = w[0];
            let (x1, y1) = w[1];
            if x <= x1 {
                let span = x1 - x0;
                if span <= 0.0 {
                    return y1;
                }
                return y0 + (x - x0) / span * (y1 - y0);
            }
        }
        knots[knots.len() - 1].1
    }
}

/// The calibrated per-callsign probability (SPEC-decode-core §4.6 as
/// amended by MAN-106): the committed calibration applied to the joint
/// raw score. The validator applies the SCP boost on top of this, capped
/// at 1.0; the raw composition stays available as `confidence::c_call`.
pub fn c_call_calibrated(char_confidences: &[f32], reps: u32, tier: PatternTier) -> f32 {
    CALIBRATION.calibrated(char_confidences, reps, tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed constants artifact the fit script writes; the Rust
    /// constants must match it exactly, so the two cannot drift.
    const CONSTANTS_JSON: &str = include_str!("../../data/calibration/constants.json");

    #[test]
    fn knots_are_ascending_and_monotone() {
        let k = CALIBRATION.knots;
        assert!(k.len() <= 8, "at most 8 fitted knots, got {}", k.len());
        assert_eq!(k[0], (0.0, 0.0), "raw 0 calibrates to 0");
        for w in k.windows(2) {
            assert!(w[1].0 > w[0].0, "raw axis must strictly increase: {k:?}");
            assert!(w[1].1 >= w[0].1, "probability must not decrease: {k:?}");
        }
        assert!(k.last().unwrap().1 <= 1.0 && k.last().unwrap().1 > 0.0);
    }

    #[test]
    fn map_is_monotone_and_bounded_over_the_domain() {
        let mut prev = CALIBRATION.map(0.0);
        assert_eq!(prev, 0.0);
        let mut x = 0.0;
        while x <= 1.0 {
            let p = CALIBRATION.map(x);
            assert!((0.0..=1.0).contains(&p), "map({x}) = {p} outside [0,1]");
            assert!(
                p >= prev,
                "map must be non-decreasing: map({x}) = {p} < {prev}"
            );
            prev = p;
            x += 1.0 / 512.0;
        }
        assert!(CALIBRATION.map(1.0) <= 1.0);
    }

    #[test]
    fn constants_match_the_committed_fit_artifact() {
        // JSON numbers vs Rust f32 literals: compare the parsed values,
        // which is the drift that matters (a stale re-fit).
        let v: serde_json::Value = serde_json::from_str(CONSTANTS_JSON).unwrap();
        let knots = v["knots"].as_array().unwrap();
        assert_eq!(knots.len(), CALIBRATION.knots.len());
        for (json, rust) in knots.iter().zip(CALIBRATION.knots) {
            assert_eq!(json[0].as_f64().unwrap() as f32, rust.0);
            assert_eq!(json[1].as_f64().unwrap() as f32, rust.1);
        }
        let tf = v["tier_factors"].as_array().unwrap();
        assert_eq!(tf.len(), 3);
        for (json, rust) in tf.iter().zip(CALIBRATION.tier_factors) {
            assert_eq!(json.as_f64().unwrap() as f32, *rust);
        }
        assert_eq!(
            v["rep_scale"].as_f64().unwrap() as f32,
            CALIBRATION.rep_scale
        );
    }

    #[test]
    fn calibrated_from_raw_recovers_calibrated() {
        for reps in [1u32, 2, 3, 5] {
            for tier in [
                PatternTier::Common,
                PatternTier::Listed,
                PatternTier::Unlisted,
            ] {
                for chars in [
                    vec![0.95f32, 0.95, 0.95, 0.95],
                    vec![0.4f32, 0.8, 0.2],
                    vec![0.1f32; 5],
                ] {
                    let raw = crate::confidence::c_call(&chars, reps);
                    let direct = CALIBRATION.calibrated(&chars, reps, tier);
                    let via_raw = CALIBRATION.calibrated_from_raw(raw, reps, tier);
                    assert!(
                        (direct - via_raw).abs() < 1e-6,
                        "recovery drifted: {direct} vs {via_raw} (reps {reps}, chars {chars:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn all_garble_word_lands_below_one_tenth() {
        // Research §4's ≈0.03 mixed-`?` case, taken to the all-`?` limit:
        // every character clamped to f32::EPSILON by `geo_mean`.
        let garble = vec![f32::EPSILON; 5];
        let p = CALIBRATION.calibrated(&garble, 2, PatternTier::Common);
        assert!(p < 0.1, "all-garble word must stay tentative, got {p}");
        // And an all-zero word is not calibration's business: the
        // zero-evidence gate abstains before calibration runs.
        assert!(!crate::confidence::has_acoustic_evidence(&[0.0; 5]));
    }

    /// Expected calibration error over the committed held-out split
    /// (10 uniform bins), the fit's out-of-sample quality gate.
    #[test]
    fn held_out_ece_is_at_most_one_fifth() {
        let rows = csv_rows(include_str!("../../data/calibration/heldout.csv"));
        assert!(!rows.is_empty(), "held-out fixture must not be empty");
        let mut bins = [[0f64; 3]; 10]; // n, sum_conf, sum_correct
        for (raw, reps, tier, correct) in rows {
            let p = CALIBRATION.calibrated_from_raw(raw, reps, tier) as f64;
            let b = (p * 10.0) as usize;
            let b = b.min(9);
            bins[b][0] += 1.0;
            bins[b][1] += p;
            bins[b][2] += correct;
        }
        let total: f64 = bins.iter().map(|b| b[0]).sum();
        let ece: f64 = bins
            .iter()
            .filter(|b| b[0] > 0.0)
            .map(|b| b[0] / total * ((b[1] / b[0]) - (b[2] / b[0])).abs())
            .sum();
        assert!(ece <= 0.20, "held-out ECE {ece:.4} exceeds 0.20");
    }

    fn csv_rows(csv: &str) -> Vec<(f32, u32, PatternTier, f64)> {
        let mut rows = Vec::new();
        for line in csv.lines().skip(1) {
            let cols: Vec<&str> = line.split(',').collect();
            if cols.len() != 4 {
                continue;
            }
            let tier = match cols[2] {
                "common" => PatternTier::Common,
                "listed" => PatternTier::Listed,
                _ => PatternTier::Unlisted,
            };
            rows.push((
                cols[0].trim().parse().unwrap(),
                cols[1].trim().parse().unwrap(),
                tier,
                cols[3].trim().parse().unwrap(),
            ));
        }
        rows
    }
}
