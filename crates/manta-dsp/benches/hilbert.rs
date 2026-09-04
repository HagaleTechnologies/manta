//! MAN-4/D2: measures the widened (511-tap, sparse-evaluated) Hilbert
//! FIR's real cost, so the Pi4 budget claim in the MAN-4 pin doc is a
//! measured number, not an estimate. Matches the existing bench convention
//! in crates/manta-engine/benches/cpu_budget.rs.

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::hilbert::HilbertTransformer;
use std::hint::black_box;

/// One second of 48 kHz audio -- the live-audio path's fixed rate
/// (manta-input::audio::TARGET_RATE_HZ).
fn one_second_48k() -> Vec<f32> {
    (0..48_000)
        .map(|i| (i as f32 * 0.031).sin())
        .collect()
}

fn bench_hilbert_48k_one_second(c: &mut Criterion) {
    let input = one_second_48k();
    c.bench_function("hilbert_48k_one_second", |b| {
        b.iter(|| {
            let mut h = HilbertTransformer::new();
            black_box(h.process(black_box(&input)));
        })
    });
}

criterion_group!(benches, bench_hilbert_48k_one_second);
criterion_main!(benches);
