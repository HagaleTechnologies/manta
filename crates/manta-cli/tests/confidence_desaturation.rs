//! MAN-106 Phase 2: after `q` reads the engine-supplied floor-based SNR,
//! spot confidence regains dynamic range across the SNR band. Baselines
//! from the research phase (measured at 65ad1bb, before the fix): the
//! pooled AWGN sample across 0/3/6/9 dB and multiple trials had 83 of
//! 100 spots at exactly 0.225 = 0.3 × 0.75. This is not a per-cell baseline
//! for the deterministic scenes below.

use manta_engine::{decode_samples, PipelineConfig};
use manta_server::rbn::RBN_REF_BW_CORRECTION_DB;
use manta_testkit::sensitivity;

const DURATION_S: f64 = 30.0;
const WPM: f32 = 25.0;
const SEED: u64 = 1;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Render the 10-station AWGN scene at `snr_db` (500 Hz), decode it, and
/// return each correct spot's confidence — the same recipe the research
/// probe and `manta bench sensitivity` use (fixed seeds, so the scene is
/// byte-identical between runs).
fn correct_spot_confidences(snr_db: f64) -> Vec<f64> {
    let noise = sensitivity::render_noise(DURATION_S, SEED, 0).unwrap();
    let series = sensitivity::series_signals(sensitivity::Condition::Awgn, WPM, 0, SEED);
    let (clean, texts) = sensitivity::render_series_chunked(&series, DURATION_S, 1).unwrap();
    let stations: Vec<(String, f64)> = texts
        .iter()
        .enumerate()
        .map(|(j, text)| {
            let call = text
                .split_whitespace()
                .nth(3)
                .expect("payload template is 'CQ CQ DE <call> <call> K'");
            (
                call.to_string(),
                sensitivity::CENTER_FREQ_HZ + sensitivity::station_offset_hz(j),
            )
        })
        .collect();
    let gain = sensitivity::gain_for_snr_2500(snr_db - RBN_REF_BW_CORRECTION_DB as f64);
    let iq = sensitivity::compose(&clean, &noise, gain);
    let report = decode_samples(
        &iq,
        sensitivity::SAMPLE_RATE_HZ,
        sensitivity::CENTER_FREQ_HZ,
        &PipelineConfig::default(),
    )
    .unwrap_or_else(|e| panic!("decoding {snr_db} dB: {e}"));
    report
        .spots
        .iter()
        .filter(|sp| {
            stations
                .iter()
                .any(|(call, freq)| sp.callsign == *call && (sp.freq_hz - freq).abs() <= 150.0)
        })
        .map(|sp| sp.confidence as f64)
        .collect()
}

#[test]
fn median_confidence_rises_with_snr_and_leaves_the_pinned_floor() {
    let at_3 = correct_spot_confidences(3.0);
    let at_9 = correct_spot_confidences(9.0);
    let at_15 = correct_spot_confidences(15.0);
    // Each cell needs correct spots for its median to be meaningful.
    assert!(!at_3.is_empty() && !at_9.is_empty() && !at_15.is_empty());
    let (m3, m9, m15) = (median(at_3.clone()), median(at_9.clone()), median(at_15));
    assert!(
        m3 < m9 && m9 < m15,
        "median correct-spot confidence must rise across the band: 3 dB {m3}, 9 dB {m9}, 15 dB {m15}"
    );
    // The pooled research baseline had 83 of 100 AWGN spots on the
    // 0.225 = 0.3 (q floor) × 0.75 (r=2) pin. This checks the new
    // 9 dB cell independently, not against a matched per-cell baseline.
    let pinned = at_9.iter().filter(|c| (**c - 0.225).abs() < 1e-6).count();
    eprintln!(
        "medians 3/9/15 dB: {m3}/{m9}/{m15}; pinned at 9 dB: {pinned}/{}",
        at_9.len()
    );
    assert!(
        (pinned as f64) < 0.10 * at_9.len() as f64,
        "{pinned} of {} spots still pinned at exactly 0.225 at 9 dB",
        at_9.len()
    );
}
