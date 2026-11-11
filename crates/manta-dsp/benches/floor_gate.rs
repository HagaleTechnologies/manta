//! MAN-526: per-second cost of the detector's per-hop floor and gate at
//! 192 kS/s (2048 channels, 375 hops/s): `power_db` for every channel,
//! `FloorBank::update`, `Gate::update`. One iteration is one second of
//! input, so its time over 1 s is the fraction of one core. Not gated in
//! CI; the decision record quotes it.

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::channelizer::power_db;
use manta_dsp::floor::{FloorBank, Gate};
use std::hint::black_box;

const N: usize = 2048;
const HOPS_PER_S: usize = 375;

/// Deterministic chi-squared (2 DOF) noise at 1e-9 plus 21 keyed carriers
/// (every channel `k % 97 == 5`, 1e-6, keyed every 40 hops).
fn hops(n_hops: usize) -> Vec<Vec<f32>> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..n_hops)
        .map(|hop| {
            (0..N)
                .map(|k| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let u = ((state >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
                    let noise = -u.ln() * 1e-9;
                    let carrier = if k % 97 == 5 && (hop / 40) % 2 == 0 {
                        1e-6
                    } else {
                        0.0
                    };
                    (noise + carrier) as f32
                })
                .collect()
        })
        .collect()
}

/// One hop through the detector's floor and gate, as `TrackManager::step_hop`
/// runs it.
fn one_hop(p: &[f32], db: &mut [f64], floor: &mut FloorBank, gate: &mut Gate) {
    for (d, &x) in db.iter_mut().zip(p) {
        *d = power_db(x);
    }
    floor.update(db, p);
    black_box(gate.update(db, p, floor));
}

fn bench_floor_gate(c: &mut Criterion) {
    let input = hops(HOPS_PER_S * 4);
    // `TrackManager::new` defaults: on 11.5 dB, off 3 dB, [38, 38] hops,
    // EMA rise at max(on, 12) dB.
    let mut floor = FloorBank::new(N);
    let mut gate = Gate::new(N, 11.5, 3.0, &[38, 38]).with_ema_rise(12.0);
    let mut db = vec![0.0; N];
    // Warm up past the 10 s ring so the floor is in steady state.
    for p in input.iter().cycle().take(250 * 15) {
        one_hop(p, &mut db, &mut floor, &mut gate);
    }
    let mut group = c.benchmark_group("floor_gate");
    group.sample_size(10);
    group.bench_function("192khz_1s_2048ch", |b| {
        let mut it = input.iter().cycle();
        b.iter(|| {
            for p in it.by_ref().take(HOPS_PER_S) {
                one_hop(p, &mut db, &mut floor, &mut gate);
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_floor_gate);
criterion_main!(benches);
