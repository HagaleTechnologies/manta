//! MAN-106 / SPEC-decode-core-v2 §8.4 stage-3 gate, V8w leg: the median
//! calibrated confidence of emitted spots exceeds the median calibrated
//! confidence of bogus candidates (withheld validator candidates whose
//! callsign no station in the scene keyed) by at least 0.3 — the ticket
//! scenario's Then-clause, against an honestly produced bogus population.
//! The B2/K5TR real-capture leg stays with MAN-116 (corpus unpublished);
//! see docs/DECISIONS/2026-10-30-man106-confidence-calibration.md.

use manta_engine::{decode_samples, PipelineConfig};
use manta_spot::confidence::calibration::CALIBRATION;
use manta_spot::{PatternTier, Verdict};
use manta_testkit::vectors;

/// The call of one payload-template signal: "CQ CQ DE <CALL> <CALL> K".
fn call_of(keyed_text: &str) -> &str {
    keyed_text
        .split_whitespace()
        .nth(3)
        .expect("pileup payloads follow the 'CQ CQ DE <CALL> <CALL> K' template")
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[test]
fn v8w_true_vs_bogus_median_confidence_separates_by_at_least_zero_point_three() {
    let spec = vectors::v8w();
    let rendered = vectors::render(&spec).unwrap();
    let report = decode_samples(
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
        &PipelineConfig::default(),
    )
    .unwrap();
    let known: Vec<&str> = rendered.keyed_texts.iter().map(|t| call_of(t)).collect();

    // Genuine: the emitted spots' calibrated confidences.
    let true_confs: Vec<f64> = report.spots.iter().map(|s| s.confidence as f64).collect();
    // Bogus: withheld/deferred candidates whose callsign no station keyed
    // (bench truth), calibrated exactly as a spot of the same evidence
    // would be — the hard negatives.
    let bogus_confs: Vec<f64> = report
        .decisions
        .iter()
        .filter(|d| d.verdict != Verdict::Spotted)
        .filter(|d| !known.contains(&d.call.as_str()))
        .filter_map(|d| {
            let raw = d.confidence?;
            let reps = d.reps.unwrap_or(1).max(1);
            let tier = d.pattern_tier.unwrap_or(PatternTier::Common);
            Some(CALIBRATION.calibrated_from_raw(raw, reps, tier) as f64)
        })
        .collect();

    assert!(
        !true_confs.is_empty(),
        "V8w must emit spots for the gate to mean anything"
    );
    assert!(
        !bogus_confs.is_empty(),
        "withheld bogus candidates must exist on V8w (the hard negatives)"
    );
    let separation = median(true_confs) - median(bogus_confs);
    assert!(
        separation >= 0.3,
        "SPEC-v2 §8.4 stage-3 V8w leg: median(true) - median(bogus) = {separation:.3} < 0.3"
    );
}
