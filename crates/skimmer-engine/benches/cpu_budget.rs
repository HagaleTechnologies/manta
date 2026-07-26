//! ROADMAP.md M2 accept criterion: full pipeline at 192 kS/s with 300
//! active tracks uses < 50% of one core on an M-series Mac AND < 1 core on
//! a Raspberry Pi 4. This is the `cargo bench` profiling target; the
//! actual Mac-budget assertion is the `#[ignore]`d test in
//! `tests/cpu_budget.rs` -- perf assertions don't belong in a criterion
//! group, and this bench isn't wired into CI (GitHub-hosted runners aren't
//! Mac-series or Pi4 hardware, and perf assertions on shared CI runners
//! are flaky). See
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.

use criterion::{criterion_group, criterion_main, Criterion};
use num_complex::Complex32;
use skimmer_engine::PipelineConfig;
use skimmer_testkit::scene::{render_scene, SignalSpec};
use std::hint::black_box;
use std::time::Duration;

/// 300 simultaneous keyed tones spread across a 192 kS/s passband, evenly
/// spaced ~600 Hz apart (well clear of the 93.75 Hz channel-merge
/// threshold). No accuracy requirement -- this only needs to drive the
/// detector into promoting ~300 concurrent ACTIVE tracks so the bench
/// exercises real channelizer + detector + decoder-pool cost, not decode
/// correctness.
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

fn bench_cpu_budget(c: &mut Criterion) {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let mut group = c.benchmark_group("cpu_budget");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(150));
    group.warm_up_time(Duration::from_secs(1));
    group.bench_function("192khz_300tracks", |b| {
        b.iter(|| {
            skimmer_engine::decode_samples(
                black_box(&iq),
                black_box(fs),
                black_box(center_freq_hz),
                black_box(&cfg),
            )
            .unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, bench_cpu_budget);
criterion_main!(benches);
