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

#[test]
fn bypass_iq_imbalance_produces_a_bogus_mirror_spot() {
    for trial in 1..=2 {
        let mut scene = iq_scene(trial);
        add_iq_imbalance(&mut scene.samples, 1.05, 2.0);
        let out = decode(&scene.samples, &bypass());
        let mirrors = mirror_spots(&scene, &out);
        eprintln!(
            "IQ imbalance trial {trial}: bypass {} clean calls, {mirrors} mirror spots",
            found(&scene, &out)
        );
        assert!(
            mirrors > 0,
            "the fixture must produce an actual false mirror spot"
        );
    }
}

fn measured_iq_calibration(
    gain_ratio: f64,
    phase_deg: f64,
) -> manta_dsp::condition::iq::IqCalibration {
    use manta_dsp::condition::iq::{IqCalibration, ToneMeter, ToneResult};
    let mut meter = ToneMeter::new(96_000.0, 12_000.0);
    for sample in iq_tone_capture(gain_ratio, phase_deg) {
        meter.push(sample);
    }
    match meter.result() {
        ToneResult::Accepted { offset_hz, rho, .. } => {
            assert!((offset_hz - 12_013.7).abs() < 0.05);
            IqCalibration::from_image_ratio(rho, 14_000_000.0)
                .expect("a valid measured calibration")
        }
        result => panic!("separate controlled-tone measurement failed: {result:?}"),
    }
}

fn iq_apply(calibration: Option<manta_dsp::condition::iq::IqCalibration>) -> PipelineConfig {
    PipelineConfig {
        conditioning: ConditioningConfig {
            iq: StageMode::Apply,
            iq_calibration: calibration,
            ..ConditioningConfig::default()
        },
        ..PipelineConfig::default()
    }
}

#[test]
fn iq_correction_from_a_measured_tone_removes_the_mirror_spot() {
    let calibration = measured_iq_calibration(1.05, 2.0);
    for trial in 1..=2 {
        let clean = iq_scene(trial);
        let want = found_calls(&clean, &decode(&clean.samples, &bypass()));
        assert_eq!(
            want.len(),
            6,
            "the clean positive control must decode every station"
        );
        let mut scene = iq_scene(trial);
        add_iq_imbalance(&mut scene.samples, 1.05, 2.0);
        let b = decode(&scene.samples, &bypass());
        assert!(
            mirror_spots(&scene, &b) > 0,
            "the bypass control must reproduce the false spot"
        );
        let out = decode(&scene.samples, &iq_apply(Some(calibration)));
        eprintln!(
            "measured IQ trial {trial}: bypass {} mirrors, corrected {} mirrors and {} calls",
            mirror_spots(&scene, &b),
            mirror_spots(&scene, &out),
            found(&scene, &out)
        );
        assert_eq!(mirror_spots(&scene, &out), 0);
        assert_eq!(found_calls(&scene, &out), want);
    }
}

#[test]
fn iq_correction_on_a_balanced_receiver_changes_no_spot() {
    let calibration = measured_iq_calibration(1.0, 0.0);
    for trial in 1..=2 {
        let scene = series_scene(45.0, trial);
        let b = decode(&scene.samples, &bypass());
        assert_eq!(
            found(&scene, &b),
            10,
            "the bypass control must decode the scene"
        );
        let out = decode(&scene.samples, &iq_apply(Some(calibration)));
        assert_eq!(mirror_spots(&scene, &out), 0);
        assert_eq!(found_calls(&scene, &out), found_calls(&scene, &b));
    }
}

#[test]
fn iq_correction_is_withheld_without_a_matching_calibration() {
    let mut calibration = measured_iq_calibration(1.05, 2.0);
    calibration.center_freq_hz += 30_000.0;
    let mut scene = iq_scene(1);
    add_iq_imbalance(&mut scene.samples, 1.05, 2.0);
    let b = decode(&scene.samples, &bypass());
    assert!(
        mirror_spots(&scene, &b) > 0,
        "the bypass control must reproduce the false spot"
    );
    for calibration in [None, Some(calibration)] {
        let cfg = iq_apply(calibration);
        let out = decode(&scene.samples, &cfg);
        assert_eq!(
            out.spots, b.spots,
            "withheld correction must preserve every bypass spot"
        );
        let log = condition_log(&scene.samples, &cfg.conditioning);
        assert_eq!(
            log.iq_status(),
            manta_dsp::condition::IqStatus::CalibrationMismatch
        );
        assert_eq!(log.count(Stage::Iq, Mark::Corrected), 0);
        assert_eq!(
            log.count(Stage::Iq, Mark::Uncertain),
            scene.samples.len() as u64
        );
    }
}
