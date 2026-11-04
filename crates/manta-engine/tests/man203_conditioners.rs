//! MAN-203: each input conditioner against bypass on its own impairment,
//! with clean controls (docs/DECISIONS/2026-11-03-man203-reversible-input-conditioning.md).
//!
//! The `bypass_` tests pin that each fixture really harms today's decoder;
//! they keep the "beats bypass" gates honest. Gates are measured counts
//! with a bypass control in the same test (MAN-203 D13), never "the bug no
//! longer happens".

#[path = "support/man203.rs"]
mod support;

use manta_dsp::condition::{Mark, Stage};
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
/// The smaller DC offset that grows a false DC track beside a +150 Hz
/// station. MAN-112's close-pair association moved this window: 0.005 no
/// longer grows one in bypass, 0.003 grows one in all four trials.
const FALSE_TRACK_DC: f32 = 0.003;

fn bypass() -> PipelineConfig {
    PipelineConfig::default()
}

fn dc_apply() -> PipelineConfig {
    PipelineConfig {
        conditioning: ConditioningConfig {
            dc: StageMode::Apply,
            ..ConditioningConfig::default()
        },
        ..PipelineConfig::default()
    }
}

fn impulse_apply() -> PipelineConfig {
    PipelineConfig {
        conditioning: ConditioningConfig {
            impulse: StageMode::Apply,
            ..ConditioningConfig::default()
        },
        ..PipelineConfig::default()
    }
}

/// The sample index of a scene's single injected impulse (5 s in).
const IMPULSE_AT: usize = 5 * 96_000;

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
        let scene = near_centre_scene(150.0, NEAR_SNR_DB, FALSE_TRACK_DC, trial);
        tracks += false_dc_tracks(&scene, &decode(&scene.samples, &bypass()));
    }
    eprintln!("bypass, DC {FALSE_TRACK_DC} at +150 Hz: {tracks} false DC tracks");
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
        let scene = near_centre_scene(150.0, NEAR_SNR_DB, FALSE_TRACK_DC, trial);
        bypass_tracks += false_dc_tracks(&scene, &decode(&scene.samples, &bypass()));
        applied_tracks += false_dc_tracks(&scene, &decode(&scene.samples, &dc_apply()));
    }
    eprintln!(
        "DC {FALSE_TRACK_DC}: false DC tracks bypass {bypass_tracks}, dc_block apply {applied_tracks}"
    );
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

#[test]
fn impulse_suppression_recovers_a_single_huge_impulse() {
    for trial in 1..=2 {
        let clean = series_scene(12.0, trial);
        let want = found_calls(&clean, &decode(&clean.samples, &bypass()));
        for v in [1e8f32, 1e9] {
            let mut scene = series_scene(12.0, trial);
            add_impulses(&mut scene.samples, &[(IMPULSE_AT, Complex32::new(v, 0.0))]);
            let b = found(&scene, &decode(&scene.samples, &bypass()));
            let applied = found_calls(&scene, &decode(&scene.samples, &impulse_apply()));
            let log = condition_log(&scene.samples, &impulse_apply().conditioning);
            eprintln!(
                "trial {trial}, one {v:e} impulse: clean {} of 10, bypass {b}, impulse_suppress apply {}",
                want.len(),
                applied.len()
            );
            assert!(b <= 2, "the fixture stopped hurting bypass ({b} of 10)");
            assert_eq!(
                applied, want,
                "suppression should find what the clean scene finds"
            );
            assert_eq!(log.count(Stage::Impulse, Mark::Repaired), 1);
        }
    }
}

#[test]
fn impulse_suppression_recovers_an_impulse_train() {
    let clean = series_scene(12.0, 1);
    let want = found_calls(&clean, &decode(&clean.samples, &bypass()));
    let mut scene = series_scene(12.0, 1);
    // 10 impulses a second of amplitude 30 (600 times the noise rms) for
    // 29 s: amplitude 10 left bypass whole, 30 cost it 9 of 10 stations.
    let train = impulse_train(10.0, DURATION_S - 1.0, Complex32::new(30.0, 0.0));
    add_impulses(&mut scene.samples, &train);
    let b = found(&scene, &decode(&scene.samples, &bypass()));
    let applied = found_calls(&scene, &decode(&scene.samples, &impulse_apply()));
    let log = condition_log(&scene.samples, &impulse_apply().conditioning);
    eprintln!(
        "{} impulses of 30: clean {} of 10, bypass {b}, impulse_suppress apply {}",
        train.len(),
        want.len(),
        applied.len()
    );
    assert!(
        b < want.len(),
        "the train should cost bypass a station ({b})"
    );
    assert_eq!(
        applied, want,
        "suppression should find what the clean scene finds"
    );
    assert_eq!(
        log.count(Stage::Impulse, Mark::Repaired),
        train.len() as u64
    );
}

#[test]
fn impulse_suppression_leaves_clean_scenes_alone() {
    for snr_db in [12.0, 45.0] {
        for trial in TRIALS {
            let scene = series_scene(snr_db, trial);
            let log = condition_log(&scene.samples, &impulse_apply().conditioning);
            let touched = log.count(Stage::Impulse, Mark::Repaired)
                + log.count(Stage::Impulse, Mark::Flagged);
            eprintln!("clean {snr_db} dB, trial {trial}: impulse stage touched {touched} samples");
            assert!(
                touched <= 16,
                "{touched} samples touched at {snr_db} dB, trial {trial}"
            );
            assert_eq!(
                log.count(Stage::Impulse, Mark::Uncertain),
                0,
                "the stage paused at {snr_db} dB, trial {trial}"
            );
        }
    }
    for (snr_db, trials) in [(12.0, 1..=1), (45.0, TRIALS)] {
        for trial in trials {
            let scene = series_scene(snr_db, trial);
            let b = found_calls(&scene, &decode(&scene.samples, &bypass()));
            let applied = found_calls(&scene, &decode(&scene.samples, &impulse_apply()));
            eprintln!(
                "clean {snr_db} dB, trial {trial}: bypass {} of 10, impulse_suppress apply {}",
                b.len(),
                applied.len()
            );
            assert_eq!(
                applied, b,
                "suppression changed the calls at {snr_db} dB, trial {trial}"
            );
        }
    }
}
