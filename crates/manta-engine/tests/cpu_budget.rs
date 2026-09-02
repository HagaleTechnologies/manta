//! ROADMAP.md M2 CPU-budget accept criterion (see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md and
//! benches/cpu_budget.rs's module doc). This test's own `assert!`s only
//! check the < 0.5x Mac wall-clock bar; it also prints a CPU-time ratio
//! (the criterion Pi4's < 1.0x budget actually is) and an active-track
//! count for a human to judge on Pi4 hardware, per
//! docs/RUNBOOKS/m2-pi4-cpu-budget.md -- that manual Pi4 run is an
//! explicitly flagged outstanding step, same pattern as M1's still-
//! outstanding W1AW live-copy run (see CLAUDE.md Status).

use manta_decode::events::DecoderEvent;
use manta_engine::PipelineConfig;
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::collections::HashSet;

/// `(user + sys)` CPU seconds consumed by this process so far, per
/// `getrusage(RUSAGE_SELF)`. Wall clock alone can't distinguish "cheap,
/// truly single-core work" from "expensive work spread thin across many
/// threads" -- see docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md.
/// The Pi4 accept criterion (< 1 full core, ROADMAP.md M2) is a CPU-time
/// budget, not a wall-clock one; this is what actually answers it.
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed");
    let to_secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0;
    to_secs(usage.ru_utime) + to_secs(usage.ru_stime)
}

fn track_id(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. } => *track_id,
    }
}

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

/// This test MUST be run with `cargo test --release` for a meaningful result.
/// Plain `cargo test` (dev profile) measures with `opt-level = 1` for first-party crates per the
/// workspace root Cargo.toml, producing dev-profile speeds that are ~1.45x slower than release
/// (0.54x realtime vs. the actual 0.360x under --release). A dev-profile run can show false
/// failures near/over the 0.5x budget even though the actual (release-profile) pipeline
/// clears it comfortably.
#[test]
#[ignore]
fn cpu_budget_mac_under_half_core() {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let audio_duration_s = iq.len() as f64 / fs;
    let cpu_before = cpu_seconds();
    let start = std::time::Instant::now();
    let report = manta_engine::decode_samples(&iq, fs, center_freq_hz, &cfg);
    let elapsed = start.elapsed().as_secs_f64();
    let cpu_elapsed = cpu_seconds() - cpu_before;
    let report = report.expect("decode_samples failed");

    // ROADMAP.md's 300-active-tracks scene only does its job if the
    // detector actually promoted ~300 tracks -- a detector/config
    // regression that promotes fewer tracks would silently benchmark a
    // cheaper workload and could pass this gate for the wrong reason.
    let active_tracks: HashSet<u32> = report.events.iter().map(track_id).collect();
    println!(
        "cpu_budget: {} unique tracks decoded (scene has 300 signals)",
        active_tracks.len()
    );
    assert!(
        active_tracks.len() >= 285,
        "only {} of 300 scene signals produced a track (>=285, 95%, required) -- \
         detector/config regression would silently cheapen this benchmark",
        active_tracks.len()
    );

    let wall_ratio = elapsed / audio_duration_s;
    // (user + sys) / audio_duration is the criterion ROADMAP.md actually
    // states ("< 1 full core" / "< 50% of one core") -- wall-clock alone
    // only equals it when the pipeline is single-core-bound, which is
    // true on Mac today (see the pins doc) but not guaranteed on Pi4's
    // weaker/differently-scheduled cores. Report both; the Pi4 runbook's
    // pass/fail check is this cpu_ratio line, not the wall-clock one.
    let cpu_ratio = cpu_elapsed / audio_duration_s;
    println!(
        "cpu_budget: {elapsed:.2}s wall / {audio_duration_s:.2}s audio = {wall_ratio:.3}x realtime wall-clock (Mac budget: < 0.5x)"
    );
    println!(
        "cpu_budget: {cpu_elapsed:.2}s (user+sys) CPU / {audio_duration_s:.2}s audio = {cpu_ratio:.3}x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)"
    );
    assert!(
        wall_ratio < 0.5,
        "192 kS/s / 300-track pipeline used {wall_ratio:.3}x realtime, Mac budget is < 0.5x (< 50% of one core)"
    );
}
