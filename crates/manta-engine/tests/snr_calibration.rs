//! MAN-102: the SNR manta reports on a spot must track the scene's true
//! SNR_2500, not the demod's keying-rail ratio it reported before (measured
//! slope 0.76, offset -3.4 dB on `main` prior to this fix -- see
//! docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md).

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

const TOLERANCE_DB: f32 = 3.0;
const FS: f64 = 96_000.0;
const CENTER_FREQ_HZ: f64 = 14_000_000.0;
const NOISE_SEED: u64 = 0x534B_494D_5631; // "SKIMV1", same seed V1 uses.

/// `None` when the scene produced no validated spot at all -- observed at
/// the high end of the sweep (30 dB) independent of this fix, and not
/// itself a MAN-102 regression: see the calibration decision record.
fn try_reported_snr_for(true_snr_2500_db: f32) -> Option<f32> {
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: true_snr_2500_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (samples, _texts) =
        render_scene(std::slice::from_ref(&sig), FS, 60.0, Some(NOISE_SEED)).unwrap();
    let report = decode_samples(&samples, FS, CENTER_FREQ_HZ, &PipelineConfig::default()).unwrap();
    if report.spots.is_empty() {
        return None;
    }
    let sum: f32 = report.spots.iter().map(|s| s.snr_db).sum();
    Some(sum / report.spots.len() as f32)
}

fn reported_snr_for(true_snr_2500_db: f32) -> f32 {
    try_reported_snr_for(true_snr_2500_db)
        .unwrap_or_else(|| panic!("no spots produced for true SNR {true_snr_2500_db} dB"))
}

#[test]
fn reported_snr_tracks_true_snr_within_3db() {
    for &true_db in &[6.0f32, 16.0, 25.0] {
        let got = reported_snr_for(true_db);
        assert!(
            (got - true_db).abs() <= TOLERANCE_DB,
            "true {true_db} dB -> reported {got:.2} dB, outside +/-{TOLERANCE_DB} dB"
        );
    }
}

/// Full characterization sweep; too slow for every CI run, kept so the
/// slope/offset can be re-measured when the estimator is touched. Same
/// `#[ignore]` pattern as `tests/cpu_budget.rs`.
#[ignore]
#[test]
fn snr_calibration_sweep_characterization() {
    println!("true_snr2500  reported");
    for &true_db in &[0.0f32, 3.0, 6.0, 10.0, 16.0, 20.0, 25.0, 30.0] {
        match try_reported_snr_for(true_db) {
            Some(got) => println!("{true_db:12.1}  {got:8.2}"),
            None => println!("{true_db:12.1}  (no spots)"),
        }
    }
}
