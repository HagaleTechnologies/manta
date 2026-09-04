//! MAN-4 D2: Pi4 CPU-budget precedent for the widened (511-tap, sparse
//! nonzero-only) Hilbert FIR -- see
//! docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md pin 6. Not wired
//! into CI (perf assertions on shared GitHub-hosted runners are flaky,
//! same rationale as crates/manta-engine/benches/cpu_budget.rs).

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::hilbert::HilbertTransformer;
use std::hint::black_box;

/// One second of 48 kHz audio -- the live-audio path's only Hilbert
/// call site (`manta-input::AudioIqSource`) runs at exactly this rate.
fn bench_hilbert_48k_one_second(c: &mut Criterion) {
    const FS: usize = 48_000;
    let input: Vec<f32> = (0..FS).map(|i| (i as f32 * 0.01).sin()).collect();
    c.bench_function("hilbert_48k_one_second", |b| {
        b.iter(|| {
            let mut h = HilbertTransformer::new();
            black_box(h.process(black_box(&input)))
        })
    });
}

criterion_group!(benches, bench_hilbert_48k_one_second);
criterion_main!(benches);
