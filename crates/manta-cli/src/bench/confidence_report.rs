//! MAN-106 Phase 3: the confidence instrument's pure arithmetic. From
//! bench-truth-labeled confidences (correct spots, bogus emitted spots
//! and withheld validator candidates as hard negatives) it computes the
//! separation, a 10-bin reliability diagram, ECE, Brier, log loss and a
//! risk-vs-coverage sweep — the ticket's "reliability diagram on testkit
//! scenes", as standing output of `manta bench sensitivity`.
//!
//! Labeling (bench truth, one rule for the summary and the fit rows):
//! `correct` is `true` for an emitted spot matching a station's call
//! within `MATCH_HZ`, and `false` for any emitted spot or withheld/
//! deferred candidate whose callsign no station in the scene sent. A
//! withheld candidate whose call IS a station's is neither (not
//! published, not bogus), so it is excluded. Each row's `score` is the
//! validator decision's raw `c_call` evidence and `p` the calibrated
//! probability `scripts/fit-confidence-calibration.py` refits from.

use manta_spot::confidence::calibration::CALIBRATION;
use manta_spot::{Decision, PatternTier, Verdict};

/// One labeled confidence row, per point.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct ConfidenceRow {
    pub call: String,
    /// The raw `c_call` evidence (`geo_mean · (1 − 0.5^r)`).
    pub score: f64,
    /// The calibrated probability the callsign is correct.
    pub p: f64,
    pub reps: u32,
    pub tier: &'static str,
    pub correct: bool,
}

/// One 10-bin reliability bin. `bin` is `[lo, hi)`, the last bin `[0.9, 1.0]`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct ReliabilityBin {
    pub bin: [f64; 2],
    pub n: usize,
    pub mean_conf: f64,
    pub accuracy: f64,
}

/// One point of the risk-vs-coverage sweep: reading the top-`k` most
/// confident spots by `coverage`, the observed error rate.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct RiskCoveragePoint {
    pub coverage: f64,
    pub risk: f64,
}

/// The per-point confidence block of the `--json` report.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct ConfidenceSummary {
    pub true_median: Option<f64>,
    pub bogus_median: Option<f64>,
    pub separation: Option<f64>,
    pub true_n: usize,
    pub bogus_n: usize,
    pub reliability: Vec<ReliabilityBin>,
    pub ece: f64,
    pub brier: Option<f64>,
    pub log_loss: Option<f64>,
    pub risk_coverage: Vec<RiskCoveragePoint>,
}

/// The median of an empty slice is `None` (no NaN out of empty classes).
pub(crate) fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// Epsilon clamp for log loss: probabilities come out of a monotone map
/// that can emit exactly 0.0.
const LOG_EPS: f64 = 1e-7;

/// The summary over labeled rows. Pure: same rows in, same numbers out.
pub(crate) fn summarize(rows: &[ConfidenceRow]) -> ConfidenceSummary {
    let trues: Vec<f64> = rows.iter().filter(|r| r.correct).map(|r| r.p).collect();
    let boguses: Vec<f64> = rows.iter().filter(|r| !r.correct).map(|r| r.p).collect();
    let true_median = median(trues.clone());
    let bogus_median = median(boguses.clone());
    let separation = match (true_median, bogus_median) {
        (Some(t), Some(b)) => Some(t - b),
        _ => None,
    };

    let mut bins = Vec::with_capacity(10);
    let mut ece = 0.0f64;
    for i in 0..10 {
        let (lo, hi) = (i as f64 / 10.0, (i + 1) as f64 / 10.0);
        let in_bin: Vec<&ConfidenceRow> = rows
            .iter()
            .filter(|r| {
                if i == 9 {
                    r.p >= lo && r.p <= 1.0
                } else {
                    r.p >= lo && r.p < hi
                }
            })
            .collect();
        if in_bin.is_empty() {
            bins.push(ReliabilityBin {
                bin: [lo, hi],
                n: 0,
                mean_conf: 0.0,
                accuracy: 0.0,
            });
            continue;
        }
        let n = in_bin.len();
        let mean_conf: f64 = in_bin.iter().map(|r| r.p).sum::<f64>() / n as f64;
        let accuracy: f64 = in_bin.iter().filter(|r| r.correct).count() as f64 / n as f64;
        ece += n as f64 / rows.len() as f64 * (accuracy - mean_conf).abs();
        bins.push(ReliabilityBin {
            bin: [lo, hi],
            n,
            mean_conf,
            accuracy,
        });
    }

    let (brier, log_loss) = if rows.is_empty() {
        (None, None)
    } else {
        let n = rows.len() as f64;
        let b = rows
            .iter()
            .map(|r| (r.p - r.correct as u8 as f64).powi(2))
            .sum::<f64>()
            / n;
        let ll = rows
            .iter()
            .map(|r| {
                let (y, p) = (r.correct as u8 as f64, r.p.clamp(LOG_EPS, 1.0 - LOG_EPS));
                -(y * p.ln() + (1.0 - y) * (1.0 - p).ln())
            })
            .sum::<f64>()
            / n;
        (Some(b), Some(ll))
    };

    // Risk-vs-coverage: sort by calibrated confidence, most confident
    // first (ties keep row order, so the sweep is deterministic); the
    // top-k prefix's error rate at every coverage k/N.
    let mut risk_coverage = Vec::new();
    let mut by_conf: Vec<&ConfidenceRow> = rows.iter().collect();
    by_conf.sort_by(|a, b| b.p.total_cmp(&a.p));
    let mut errors = 0usize;
    for (k, r) in by_conf.iter().enumerate() {
        if !r.correct {
            errors += 1;
        }
        risk_coverage.push(RiskCoveragePoint {
            coverage: (k + 1) as f64 / by_conf.len() as f64,
            risk: errors as f64 / (k + 1) as f64,
        });
    }

    ConfidenceSummary {
        true_median,
        bogus_median,
        separation,
        true_n: trues.len(),
        bogus_n: boguses.len(),
        reliability: bins,
        ece,
        brier,
        log_loss,
        risk_coverage,
    }
}

/// The per-point labeled rows for one decode: bench truth over the
/// emitted spots and the validator's decision log (hard negatives).
pub(crate) fn label_point(
    stations: &[super::Station],
    spots: &[manta_spot::Spot],
    decisions: &[Decision],
) -> Vec<ConfidenceRow> {
    let is_station_call = |call: &str| stations.iter().any(|st| st.call == call);
    let mut rows = Vec::new();
    for spot in spots {
        let correct = stations.iter().any(|st| {
            spot.callsign == st.call && (spot.freq_hz - st.freq_hz).abs() <= super::MATCH_HZ
        });
        rows.push(ConfidenceRow {
            call: spot.callsign.clone(),
            score: f64::NAN, // enriched from the Spotted decision below
            p: spot.confidence as f64,
            reps: 0,
            tier: "",
            correct,
        });
    }
    for d in decisions {
        let Some(raw) = d.confidence else { continue };
        let reps = d.reps.unwrap_or(1).max(1);
        let tier = d.pattern_tier.unwrap_or(PatternTier::Common);
        let p = CALIBRATION.calibrated_from_raw(raw, reps, tier) as f64;
        if d.verdict == Verdict::Spotted {
            // Enrich the emitted spot's row with the decision's raw
            // evidence; the unmatched case (no spot row) still records a
            // labeled row.
            match rows.iter_mut().find(|r| r.call == d.call && r.reps == 0) {
                Some(row) => {
                    row.score = raw as f64;
                    row.reps = reps;
                    row.tier = tier.as_str();
                }
                None => rows.push(ConfidenceRow {
                    call: d.call.clone(),
                    score: raw as f64,
                    p,
                    reps,
                    tier: tier.as_str(),
                    correct: true,
                }),
            }
            continue;
        }
        // Withheld or deferred: bogus iff no station sent this call; a
        // station's own call, withheld (e.g. awaiting repetitions), is
        // neither published nor bogus -- excluded.
        if is_station_call(&d.call) {
            continue;
        }
        rows.push(ConfidenceRow {
            call: d.call.clone(),
            score: raw as f64,
            p,
            reps,
            tier: tier.as_str(),
            correct: false,
        });
    }
    rows
}

/// The `--reliability` stderr table: one block per point.
pub(crate) fn render_reliability(result: &super::SweepResult) -> String {
    let mut out = String::from(
        "confidence reliability per point (bins are [lo, hi) over the calibrated probability)\n",
    );
    for p in &result.points {
        let summary = summarize(&p.confidence_rows);
        out.push_str(&format!(
            "\n{} {} WPM {} dB: true n={} median={:?}, bogus n={} median={:?}, separation={:?}, ECE {:.3}, Brier {:?}, log loss {:?}\n",
            p.condition.label(),
            p.wpm,
            super::fmt_db(p.snr_db),
            summary.true_n,
            summary.true_median.map(|x| format!("{x:.3}")),
            summary.bogus_n,
            summary.bogus_median.map(|x| format!("{x:.3}")),
            summary.separation.map(|x| format!("{x:.3}")),
            summary.ece,
            summary.brier.map(|x| format!("{x:.3}")),
            summary.log_loss.map(|x| format!("{x:.3}")),
        ));
        for b in &summary.reliability {
            if b.n > 0 {
                out.push_str(&format!(
                    "  [{:.1}, {:.1}): n={} mean conf {:.3} accuracy {:.3}\n",
                    b.bin[0], b.bin[1], b.n, b.mean_conf, b.accuracy
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(p: f64, correct: bool) -> ConfidenceRow {
        ConfidenceRow {
            call: "TEST".into(),
            score: p,
            p,
            reps: 2,
            tier: "common",
            correct,
        }
    }

    /// The plan's known-answer case: 3 correct at 0.9, 1 bogus at 0.2,
    /// 2 bogus at 0.1/0.15 — plus hand-computed bin/ECE/Brier/log-loss
    /// and risk-coverage values.
    #[test]
    fn reliability_arithmetic_matches_hand_computed_values() {
        let rows = vec![
            row(0.9, true),
            row(0.9, true),
            row(0.9, true),
            row(0.2, false),
            row(0.1, false),
            row(0.15, false),
        ];
        let s = summarize(&rows);
        assert_eq!(s.true_n, 3);
        assert_eq!(s.bogus_n, 3);
        assert_eq!(s.true_median, Some(0.9));
        // bogus: [0.1, 0.15, 0.2] -> median 0.15
        assert_eq!(s.bogus_median, Some(0.15));
        assert_eq!(s.separation, Some(0.9 - 0.15));
        // bin [0.9, 1.0]: n=3, mean 0.9, accuracy 1.0
        let last = &s.reliability[9];
        assert_eq!(last.n, 3);
        assert!((last.mean_conf - 0.9).abs() < 1e-12);
        assert!((last.accuracy - 1.0).abs() < 1e-12);
        // bin [0.1, 0.2): 0.1, 0.15 -> n=2, mean 0.125, accuracy 0
        let b1 = &s.reliability[1];
        assert_eq!(b1.n, 2);
        assert!((b1.mean_conf - 0.125).abs() < 1e-12);
        assert_eq!(b1.accuracy, 0.0);
        // bin [0.2, 0.3): the bogus 0.2
        assert_eq!(s.reliability[2].n, 1);
        // ECE = (3/6)|1 - 0.9| + (2/6)|0 - 0.125| + (1/6)|0 - 0.2|
        let ece = 0.5 * 0.1 + (2.0 / 6.0) * 0.125 + (1.0 / 6.0) * 0.2;
        assert!((s.ece - ece).abs() < 1e-12, "{} vs {}", s.ece, ece);
        // Brier = mean((p - y)^2) = (3*(0.1)^2 + 0.2^2 + 0.1^2 + 0.15^2)/6
        let brier = (3.0 * 0.01 + 0.04 + 0.01 + 0.0225) / 6.0;
        assert!((s.brier.unwrap() - brier).abs() < 1e-12);
        // log loss with epsilon-clamped logs
        let ll = -(3.0 * 0.9f64.ln() + 0.8f64.ln() + 0.9f64.ln() + 0.85f64.ln()) / 6.0;
        assert!((s.log_loss.unwrap() - ll).abs() < 1e-9);
        // risk-coverage: sorted desc 0.9, 0.9, 0.9, 0.2, 0.15, 0.1
        assert_eq!(s.risk_coverage.len(), 6);
        assert_eq!(s.risk_coverage[0].coverage, 1.0 / 6.0);
        assert_eq!(s.risk_coverage[0].risk, 0.0);
        assert!((s.risk_coverage[2].coverage - 0.5).abs() < 1e-12);
        assert_eq!(s.risk_coverage[2].risk, 0.0);
        assert_eq!(s.risk_coverage[3].risk, 0.25);
        assert!((s.risk_coverage[5].coverage - 1.0).abs() < 1e-12);
        assert!((s.risk_coverage[5].risk - 0.5).abs() < 1e-12);
    }

    #[test]
    fn empty_classes_yield_none_not_nan() {
        let s = summarize(&[]);
        assert_eq!(s.true_median, None);
        assert_eq!(s.bogus_median, None);
        assert_eq!(s.separation, None);
        assert_eq!(s.brier, None);
        assert_eq!(s.log_loss, None);
        assert!(s.risk_coverage.is_empty());
        assert_eq!(s.ece, 0.0);

        let only_true = summarize(&[row(0.8, true)]);
        assert_eq!(only_true.bogus_median, None);
        assert_eq!(only_true.separation, None);
        assert!(only_true.brier.is_some() && only_true.log_loss.is_some());
        let only_bogus = summarize(&[row(0.3, false)]);
        assert_eq!(only_bogus.true_median, None);
        assert_eq!(only_bogus.separation, None);
    }

    #[test]
    fn even_count_median_averages_the_middle_pair() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![0.2]), Some(0.2));
        assert_eq!(median(vec![0.3, 0.1]), Some(0.2));
        assert_eq!(median(vec![0.1, 0.4, 0.2, 0.3]), Some(0.25));
    }
}
