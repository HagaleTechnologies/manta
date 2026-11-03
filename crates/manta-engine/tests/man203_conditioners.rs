//! MAN-203: each input conditioner against bypass on its own impairment,
//! with clean controls (docs/DECISIONS/2026-11-03-man203-reversible-input-conditioning.md).
//!
//! The `bypass_` tests pin that each fixture really harms today's decoder;
//! they keep the "beats bypass" gates honest. Gates are measured counts
//! with a bypass control in the same test (MAN-203 D13), never "the bug no
//! longer happens".

#[path = "support/man203.rs"]
mod support;

use manta_engine::{ConditioningConfig, PipelineConfig, StageMode};
use num_complex::Complex32;
use support::*;

/// The near-centre offsets the DC gates use, Hz.
const NEAR_OFFSETS_HZ: [f64; 2] = [60.0, 150.0];
/// Trials 1..=4 (D13: trial 0 has A-prefix calls that never spot).
const TRIALS: std::ops::RangeInclusive<u32> = 1..=4;
/// The station SNR (2500 Hz reference) of a DC scene.
const NEAR_SNR_DB: f64 = 6.0;
/// The DC offset `(DC, 0.7 DC)` that hides a near-centre station.
const DC: f32 = 0.01;

fn bypass() -> PipelineConfig {
    PipelineConfig::default()
}

fn dc_apply() -> PipelineConfig {
    PipelineConfig {
        conditioning: ConditioningConfig {
            dc: StageMode::Apply,
        },
        ..PipelineConfig::default()
    }
}

/// Stations found over the near-centre offsets and trials, for each config.
fn near_centre_found(dc: f32, cfgs: &[&PipelineConfig]) -> Vec<(usize, Vec<String>)> {
    let mut totals = vec![(0usize, Vec::new()); cfgs.len()];
    for &offset in &NEAR_OFFSETS_HZ {
        for trial in TRIALS {
            let scene = near_centre_scene(offset, NEAR_SNR_DB, dc, trial);
            for (i, cfg) in cfgs.iter().enumerate() {
                let out = decode(&scene.samples, cfg);
                totals[i].0 += found(&scene, &out);
                totals[i].1.extend(
                    found_calls(&scene, &out)
                        .into_iter()
                        .map(|c| format!("{offset}:{c}")),
                );
            }
        }
    }
    totals
}

#[test]
fn bypass_loses_a_station_near_a_dc_offset() {
    let found = near_centre_found(DC, &[&bypass()])[0].0;
    eprintln!("bypass, DC {DC}: found {found} of 8");
    assert!(
        found <= 1,
        "bypass found {found} of 8 near-centre stations under DC {DC}"
    );
}

#[test]
fn bypass_grows_a_false_dc_track_at_a_small_offset() {
    let mut tracks = 0;
    for trial in TRIALS {
        let scene = near_centre_scene(150.0, NEAR_SNR_DB, 0.005, trial);
        tracks += false_dc_tracks(&scene, &decode(&scene.samples, &bypass()));
    }
    eprintln!("bypass, DC 0.005 at +150 Hz: {tracks} false DC tracks");
    assert!(tracks >= 3, "bypass grew only {tracks} false DC tracks");
}

#[test]
fn bypass_is_silenced_by_one_huge_impulse() {
    let mut scene = series_scene(12.0, 1);
    add_impulses(
        &mut scene.samples,
        &[(5 * 96_000, Complex32::new(1e8, 0.0))],
    );
    let found = found(&scene, &decode(&scene.samples, &bypass()));
    eprintln!("bypass, one 1e8 impulse: found {found} of 10");
    assert!(found <= 2, "bypass found {found} of 10 after a 1e8 impulse");
}

/// Gherkin scenario 1: "the offset scene's decode recall improves over
/// bypass".
#[test]
fn dc_removal_recovers_a_station_near_a_dc_offset() {
    let r = near_centre_found(DC, &[&bypass(), &dc_apply()]);
    let (bypass_found, applied_found) = (r[0].0, r[1].0);
    eprintln!("DC {DC}: bypass {bypass_found} of 8, dc_block apply {applied_found} of 8");
    assert!(
        applied_found > bypass_found,
        "DC removal found {applied_found}, bypass {bypass_found}"
    );
    assert_eq!(
        applied_found, 8,
        "DC removal should find every near-centre station"
    );
}

/// Gherkin scenario 1: "the control scene's decode recall does not
/// regress".
#[test]
fn dc_removal_does_not_regress_the_clean_control() {
    let r = near_centre_found(0.0, &[&bypass(), &dc_apply()]);
    eprintln!(
        "no DC: bypass {} of 8, dc_block apply {} of 8",
        r[0].0, r[1].0
    );
    assert!(r[1].0 >= r[0].0, "DC removal regressed the clean control");
    assert_eq!(
        r[1].1, r[0].1,
        "DC removal changed the clean control's calls"
    );

    // A station far from the centre is found with DC removal under DC.
    let mut far = 0;
    for trial in TRIALS {
        let scene = near_centre_scene(900.0, NEAR_SNR_DB, DC, trial);
        far += found(&scene, &decode(&scene.samples, &dc_apply()));
    }
    eprintln!("+900 Hz, DC {DC}, dc_block apply: {far} of 4");
    assert_eq!(far, 4);
}

#[test]
fn dc_removal_removes_the_false_dc_track() {
    let (mut bypass_tracks, mut applied_tracks) = (0, 0);
    for trial in TRIALS {
        let scene = near_centre_scene(150.0, NEAR_SNR_DB, 0.005, trial);
        bypass_tracks += false_dc_tracks(&scene, &decode(&scene.samples, &bypass()));
        applied_tracks += false_dc_tracks(&scene, &decode(&scene.samples, &dc_apply()));
    }
    eprintln!("DC 0.005: false DC tracks bypass {bypass_tracks}, dc_block apply {applied_tracks}");
    assert!(
        bypass_tracks >= 3,
        "the fixture lost its false DC track ({bypass_tracks})"
    );
    assert_eq!(applied_tracks, 0);
}

#[test]
fn dc_apply_alone_is_no_worse_than_bypass_on_a_huge_impulse() {
    let mut scene = series_scene(12.0, 1);
    add_impulses(
        &mut scene.samples,
        &[(5 * 96_000, Complex32::new(1e8, 0.0))],
    );
    let b = found(&scene, &decode(&scene.samples, &bypass()));
    let d = found(&scene, &decode(&scene.samples, &dc_apply()));
    eprintln!("one 1e8 impulse: bypass {b} of 10, dc_block apply {d} of 10");
    assert!(d >= b, "DC removal found {d}, bypass {b}");
}
