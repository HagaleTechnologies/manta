//! MAN-204 acceptance: guarded two-stage cancellation recovers a weak CW
//! station 40 Hz beside a station 20 dB stronger, which the unchanged
//! pipeline never decodes, and spots no artifact-only identity in a
//! single-signal control.
//!
//! Gherkin: Given (`support::pair` / `support::alone`), When
//! (`decode_with_cancellation`), Then (the `weak_station_*` tests: original
//! 0 weak calls, residual at least 7 of 9), And (the `single_signal_*`
//! tests). See docs/DECISIONS/2026-11-03-man204-guarded-two-stage-cancellation.md.

#[path = "support/man204.rs"]
mod support;

use manta_dsp::cancel::{estimate_strong, subtract, Hypothesis};
use manta_engine::cancel::{
    decode_with_cancellation, CancelOptions, CancellationReport, CandidateOutcome,
};
use manta_engine::{decode_samples, NoSignal, PipelineConfig};
use num_complex::Complex32;
use support::*;

/// The decision record's acceptance floor (D10): at least 7 of the 9 weak
/// calls B keys in 60 s.
const MIN_WEAK_CALLS: usize = 7;

fn run(scene: &[Complex32], cfg: &PipelineConfig, opts: &CancelOptions) -> CancellationReport {
    decode_with_cancellation(scene, FS, CENTER, cfg, opts).unwrap()
}

fn residual_calls(r: &CancellationReport, call: &str) -> usize {
    r.residual.as_ref().map_or(0, |b| calls(&b.events, call))
}

/// The weak station is recovered: the baseline decodes none of its calls,
/// the candidate is a subtracted `TwoSignalCw` at the right offset, the
/// residual decodes at least `MIN_WEAK_CALLS`, and `recovered` holds exactly
/// `K5ARH` near B's frequency.
fn assert_recovered(r: &CancellationReport, b_off: f64) {
    assert_eq!(calls(&r.original.events, B_CALL), 0, "baseline");
    assert!(
        r.original.spots.iter().any(|s| s.callsign == A_CALL),
        "original spots {:?}",
        r.original.spots
    );
    let c = &r.candidates[0];
    match c.hypothesis {
        Some(Hypothesis::TwoSignalCw { offset_hz }) => assert!(
            (c.carrier_hz + offset_hz - b_off).abs() <= 2.0,
            "evidence offset {offset_hz} from {}: {c:?}",
            c.carrier_hz
        ),
        h => panic!("hypothesis {h:?}: {c:?}"),
    }
    assert_eq!(c.outcome, CandidateOutcome::Subtracted, "{c:?}");
    let weak = residual_calls(r, B_CALL);
    assert!(
        weak >= MIN_WEAK_CALLS,
        "weak calls {weak}: {:?}",
        r.residual
    );
    assert_eq!(r.recovered.len(), 1, "{:?} / {:?}", r.recovered, r.rejected);
    assert_eq!(r.recovered[0].callsign, B_CALL);
    assert!(
        (r.recovered[0].freq_hz - (CENTER + b_off)).abs() <= 15.0,
        "{:?}",
        r.recovered
    );
}

fn reference_pair(seed: u64) {
    let scene = pair(A_OFF, 30.0, A_OFF + 40.0, 10.0, seed);
    let r = run(
        &scene,
        &PipelineConfig::default(),
        &CancelOptions::default(),
    );
    assert_recovered(&r, A_OFF + 40.0);
}

#[test]
fn weak_station_beside_a_strong_one_is_recovered_seed_204() {
    reference_pair(204);
}

#[test]
fn weak_station_beside_a_strong_one_is_recovered_seed_2() {
    reference_pair(2);
}

#[test]
fn weak_station_beside_a_strong_one_is_recovered_seed_3() {
    reference_pair(3);
}

#[test]
fn single_signal_control_recovers_nothing() {
    let scene = alone(A_OFF, 30.0, 204);
    let r = run(
        &scene,
        &PipelineConfig::default(),
        &CancelOptions::default(),
    );
    assert_eq!(r.candidates[0].hypothesis, Some(Hypothesis::OneSignal));
    assert!(r.residual.is_none());
    assert!(r.recovered.is_empty());
}

#[test]
fn single_signal_control_forced_subtraction_spots_nothing() {
    let cfg = PipelineConfig::default();
    let scene = alone(A_OFF, 30.0, 204);
    let original = decode_samples(&scene, FS, CENTER, &cfg).unwrap();
    let spot = original
        .spots
        .iter()
        .find(|s| s.callsign == A_CALL)
        .expect("W1AW spot");
    // Both gates bypassed: estimate, subtract and decode directly.
    let est = estimate_strong(&scene, FS, spot.freq_hz - CENTER);
    let mut residual = scene.clone();
    subtract(&mut residual, FS, &est);
    match decode_samples(&residual, FS, CENTER, &cfg) {
        Err(e) => assert!(e.downcast_ref::<NoSignal>().is_some(), "{e}"),
        Ok(r) => {
            assert!(r.spots.is_empty(), "{:?}", r.spots);
            assert_eq!(calls(&r.events, A_CALL), 0, "{:?}", texts(&r.events));
        }
    }
    let opts = CancelOptions {
        evidence_gate: false,
        ..Default::default()
    };
    let r = run(&scene, &cfg, &opts);
    assert!(r.recovered.is_empty(), "{:?}", r.recovered);
}

#[test]
fn original_branch_is_identical_to_decode_samples() {
    let cfg = PipelineConfig::default();
    let scene = pair(A_OFF, 30.0, A_OFF + 40.0, 10.0, 204);
    let r = run(&scene, &cfg, &CancelOptions::default());
    let plain = decode_samples(&scene, FS, CENTER, &cfg).unwrap();
    assert_eq!(
        serde_json::to_value(&r.original).unwrap(),
        serde_json::to_value(&plain).unwrap()
    );
    assert_eq!(r.original.spots, plain.spots);
}

#[test]
fn weak_station_below_the_strong_one_is_recovered() {
    let scene = pair(A_OFF, 30.0, A_OFF - 40.0, 10.0, 204);
    let r = run(
        &scene,
        &PipelineConfig::default(),
        &CancelOptions::default(),
    );
    assert_recovered(&r, A_OFF - 40.0);
}

#[test]
fn mid_channel_strong_station_is_recovered() {
    let a_off = A_OFF + MID;
    let scene = pair(a_off, 30.0, a_off + 40.0, 10.0, 204);
    let r = run(
        &scene,
        &PipelineConfig::default(),
        &CancelOptions::default(),
    );
    assert_recovered(&r, a_off + 40.0);
}

#[test]
fn steady_carrier_beside_a_strong_station_is_not_cancelled() {
    // Plan finding 8: with `non_cw_filter` on, the original branch loses A
    // entirely in this scene (a pre-existing MAN-313 defect, a follow-up),
    // so this test turns the filter off to reach the evidence gate.
    let mut cfg = PipelineConfig::default();
    cfg.detector.non_cw_filter = false;
    let scene = sum(&[
        render_station(&a(A_OFF, 30.0), DUR),
        steady(A_OFF + 40.0, 10.0, DUR),
        noise(DUR, 204),
    ]);
    let r = run(&scene, &cfg, &CancelOptions::default());
    assert_eq!(
        r.candidates[0].hypothesis,
        Some(Hypothesis::NonCw),
        "{:?}",
        r.candidates
    );
    assert!(r.residual.is_none());
    assert!(r.recovered.is_empty());
}

#[test]
fn a_carrier_hint_three_hertz_off_still_recovers() {
    let scene = pair(A_OFF, 30.0, A_OFF + 40.0, 10.0, 204);
    let opts = CancelOptions {
        carrier_hint_override_hz: Some(A_OFF + 3.0),
        ..Default::default()
    };
    let r = run(&scene, &PipelineConfig::default(), &opts);
    assert!(
        (r.candidates[0].carrier_hz - A_OFF).abs() <= 0.1,
        "{:?}",
        r.candidates
    );
    assert_recovered(&r, A_OFF + 40.0);
}
