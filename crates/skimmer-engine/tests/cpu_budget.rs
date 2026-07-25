//! ROADMAP.md M2 CPU-budget accept criterion, Mac leg only (see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md and
//! benches/cpu_budget.rs's module doc). Raspberry Pi 4 leg (< 1 core) is
//! an explicitly flagged outstanding manual step -- same pattern as M1's
//! still-outstanding W1AW live-copy run (see CLAUDE.md Status).

use num_complex::Complex32;
use skimmer_engine::PipelineConfig;
use skimmer_testkit::scene::{render_scene, SignalSpec};

fn cpu_budget_scene() -> (Vec<Complex32>, f64, f64, PipelineConfig) {
    const FS: f64 = 192_000.0;
    const CENTER_FREQ_HZ: f64 = 14_000_000.0;
    const DURATION_S: f64 = 15.0;
    const N_SIGNALS: usize = 300;

    let signals: Vec<SignalSpec> = (0..N_SIGNALS)
        .map(|i| {
            let offset_hz = -90_000.0 + i as f64 * (180_000.0 / (N_SIGNALS - 1) as f64);
            SignalSpec {
                text: "CQ CQ DE K1BNC K1BNC K".into(),
                loop_text: true,
                wpm: 20.0,
                offset_hz,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            }
        })
        .collect();
    let (samples, _texts) =
        render_scene(&signals, FS, DURATION_S, Some(0x4350_555F_4250_5431)).unwrap();
    (samples, FS, CENTER_FREQ_HZ, PipelineConfig::default())
}

#[test]
#[ignore]
fn cpu_budget_mac_under_half_core() {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let audio_duration_s = iq.len() as f64 / fs;
    let start = std::time::Instant::now();
    let report = skimmer_engine::decode_samples(&iq, fs, center_freq_hz, &cfg);
    let elapsed = start.elapsed().as_secs_f64();
    assert!(report.is_ok(), "decode_samples failed: {:?}", report.err());
    let ratio = elapsed / audio_duration_s;
    println!(
        "cpu_budget: {elapsed:.2}s wall / {audio_duration_s:.2}s audio = {ratio:.3}x realtime (Mac budget: < 0.5x)"
    );
    assert!(
        ratio < 0.5,
        "192 kS/s / 300-track pipeline used {ratio:.3}x realtime, Mac budget is < 0.5x (< 50% of one core)"
    );
}
