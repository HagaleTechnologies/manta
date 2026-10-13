//! MAN-314: `FskDetector` cost at 192 kHz (n = 2048 channels, 375 hops/s),
//! 60 s of hops carrying ten RTTY-like antiphase pairs over noise. Not
//! gated in CI; the decision record quotes it against the channelizer.

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::channelizer::{power_db, HopOutput};
use manta_dsp::floor::FloorBank;
use manta_dsp::fsk::{FskDetector, BLOCK_HOPS};
use num_complex::Complex32;
use std::hint::black_box;

const N: usize = 2048;
const SECONDS: usize = 60;

/// One block of synthetic hops: deterministic LCG noise plus ten pairs
/// two channels apart keyed in antiphase every 8 hops.
fn block() -> Vec<(HopOutput, Vec<f64>)> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    (0..BLOCK_HOPS as u64)
        .map(|m| {
            let mut x: Vec<Complex32> = (0..N).map(|_| Complex32::new(noise(), noise())).collect();
            for p in 0..10 {
                let k = 100 + p * 150 + if (m / 8).is_multiple_of(2) { 0 } else { 2 };
                x[k] = Complex32::new(30.0, 0.0);
            }
            let power: Vec<f32> = x.iter().map(|c| c.norm_sqr()).collect();
            let db = power.iter().map(|&p| power_db(p)).collect();
            (HopOutput { m, x, power }, db)
        })
        .collect()
}

fn bench_fsk(c: &mut Criterion) {
    let hops = block();
    let mut floor = FloorBank::new(N);
    for (_, db) in hops.iter().cycle().take(BLOCK_HOPS * 20) {
        floor.update(db);
    }
    let mut group = c.benchmark_group("fsk");
    group.sample_size(10);
    group.bench_function("192khz_60s_10rtty", |b| {
        b.iter(|| {
            let mut det = FskDetector::new(N, 20);
            for (h, db) in hops.iter().cycle().take(BLOCK_HOPS * SECONDS) {
                black_box(det.push_hop(h, db, &floor));
            }
            black_box(det.signals().len())
        })
    });
    group.finish();
}

criterion_group!(benches, bench_fsk);
criterion_main!(benches);
