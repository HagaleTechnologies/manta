//! MAN-273: cost of the live-audio Hilbert front end per second of 48 kHz
//! audio (`manta-input::AudioIqSource`'s rate): the legacy 129-tap design,
//! the 511-tap sparse default, and a dense 511-tap loop for context. Not
//! gated in CI; the decision record quotes the measured times.

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::hilbert::{design_hilbert_fir_n, HilbertTransformer, HILBERT_TAPS_M1_LEGACY};
use std::hint::black_box;

const FS: usize = 48_000;

fn input() -> Vec<f32> {
    (0..FS).map(|i| (i as f32 * 0.01).sin()).collect()
}

fn bench_hilbert(c: &mut Criterion) {
    let x = input();
    c.bench_function("hilbert_129_legacy_48k_1s", |b| {
        b.iter(|| HilbertTransformer::with_taps(HILBERT_TAPS_M1_LEGACY).process(black_box(&x)))
    });
    c.bench_function("hilbert_511_sparse_48k_1s", |b| {
        b.iter(|| HilbertTransformer::new().process(black_box(&x)))
    });
    let taps = design_hilbert_fir_n(511);
    c.bench_function("hilbert_511_dense_48k_1s", |b| {
        b.iter(|| {
            let mut hist = std::collections::VecDeque::from(vec![0.0f32; taps.len()]);
            let mut out = Vec::with_capacity(x.len());
            for &s in black_box(&x) {
                hist.pop_front();
                hist.push_back(s);
                let mut acc = 0.0f64;
                for (&h, &v) in taps.iter().zip(hist.iter()) {
                    acc += h as f64 * v as f64;
                }
                out.push(acc as f32);
            }
            out
        })
    });
}

criterion_group!(benches, bench_hilbert);
criterion_main!(benches);
