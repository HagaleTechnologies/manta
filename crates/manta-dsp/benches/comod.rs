//! MAN-313: `CoModDetector` cost at 192 kHz (n = 2048 channels, 375
//! hops/s), 60 s of hops. Scene (a): ten 30-channel co-modulated blocks
//! plus 300 independently keyed tones over noise. Scene (b): every channel
//! hot (worst case: every pair at every lag is correlated). A noise-only
//! baseline isolates the ring-write and mean/peak cost. The rows are a
//! precomputed 5 s chunk replayed 12 times. Not gated in CI; the decision
//! record quotes time per 60 s iteration / 60 s as the realtime fraction.

use criterion::{criterion_group, criterion_main, Criterion};
use manta_dsp::comod::CoModDetector;
use std::hint::black_box;

const N: usize = 2048;
const HOPS_PER_S: usize = 375;
const CHUNK_S: usize = 5;
const SECONDS: usize = 60;

/// Deterministic LCG.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 31) as f64
    }
    /// Uniform integer in [lo, hi].
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// On/off key: `syllable` draws 31..=62-hop runs (3-6 Hz speech
/// envelope); otherwise Morse at `dit` hops.
struct Key {
    syllable: bool,
    dit: u64,
    on: bool,
    left: u64,
    rng: Lcg,
}

impl Key {
    fn new(syllable: bool, dit: u64, seed: u64) -> Self {
        Key {
            syllable,
            dit,
            on: false,
            left: 0,
            rng: Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
        }
    }
    fn step(&mut self) -> bool {
        if self.left == 0 {
            self.on = !self.on;
            self.left = if self.syllable {
                self.rng.range(31, 62)
            } else if self.on {
                [1, 3][self.rng.range(0, 1) as usize] * self.dit
            } else {
                [1, 1, 3, 7][self.rng.range(0, 3) as usize] * self.dit
            };
        }
        self.left -= 1;
        self.on
    }
}

/// One `CHUNK_S` chunk of FFT-order SNR rows. `fill` writes one hop's
/// frequency-order row on top of +/-1.5 dB noise.
fn chunk(mut fill: impl FnMut(&mut [f64])) -> Vec<Vec<f64>> {
    let mut noise = Lcg(0x2545_f491_4f6c_dd1d);
    let mut out = vec![vec![0.0; N]; HOPS_PER_S * CHUNK_S];
    let mut row_j = vec![0.0; N];
    for row_k in out.iter_mut() {
        for v in row_j.iter_mut() {
            *v = (2.0 * noise.unit() - 1.0) * 1.5;
        }
        fill(&mut row_j);
        for (j, &v) in row_j.iter().enumerate() {
            row_k[(j + N - N / 2) % N] = v;
        }
    }
    out
}

/// Scene (a): ten 30-channel speech-like blocks 200 channels apart, and 300
/// Morse tones five channels apart in the gaps, each at its own speed.
fn scene_blocks_and_tones() -> Vec<Vec<f64>> {
    let mut rng = Lcg(42);
    let mut blocks: Vec<(usize, Vec<f64>, Key)> = (0..10)
        .map(|b| {
            let levels = (0..30).map(|_| 15.0 + 10.0 * rng.unit()).collect();
            (60 + b * 200, levels, Key::new(true, 0, 1000 + b as u64))
        })
        .collect();
    let in_block = |j: usize| blocks.iter().any(|(f, _, _)| j + 5 >= *f && j < f + 35);
    let tone_js: Vec<usize> = (0..N)
        .step_by(5)
        .filter(|&j| !in_block(j))
        .take(300)
        .collect();
    assert_eq!(tone_js.len(), 300);
    let mut tones: Vec<(usize, f64, Key)> = tone_js
        .iter()
        .enumerate()
        .map(|(i, &j)| {
            let dit = rng.range(14, 24);
            (
                j,
                20.0 + 10.0 * rng.unit(),
                Key::new(false, dit, 5000 + i as u64),
            )
        })
        .collect();
    chunk(|row| {
        for (first, levels, key) in blocks.iter_mut() {
            if key.step() {
                for (i, &l) in levels.iter().enumerate() {
                    row[*first + i] += l;
                }
            }
        }
        for (j, level, key) in tones.iter_mut() {
            if key.step() {
                row[*j] += *level;
            }
        }
    })
}

/// Scene (b): every channel keyed independently, 24 dB on over an 8 dB
/// pedestal, so every channel's window mean stays hot.
fn scene_all_hot() -> Vec<Vec<f64>> {
    let mut rng = Lcg(43);
    let mut keys: Vec<Key> = (0..N)
        .map(|j| Key::new(false, rng.range(14, 24), 9000 + j as u64))
        .collect();
    chunk(|row| {
        for (v, key) in row.iter_mut().zip(keys.iter_mut()) {
            *v += 8.0 + if key.step() { 16.0 } else { 0.0 };
        }
    })
}

/// Baseline: noise only, no hot channel, so only the ring writes and the
/// per-channel mean/peak pass run.
fn scene_quiet() -> Vec<Vec<f64>> {
    chunk(|_| {})
}

fn run(rows: &[Vec<f64>]) -> usize {
    let mut det = CoModDetector::new(N);
    let mut evals = 0;
    for row in rows.iter().cycle().take(HOPS_PER_S * SECONDS) {
        evals += usize::from(det.push_hop(black_box(row)));
    }
    black_box(det.spans().len()) + evals
}

fn bench_comod(c: &mut Criterion) {
    let a = scene_blocks_and_tones();
    let b = scene_all_hot();
    let mut group = c.benchmark_group("comod");
    group.sample_size(10);
    group.bench_function("192khz_60s_10blocks_300tones", |bch| bch.iter(|| run(&a)));
    group.bench_function("192khz_60s_all_hot", |bch| bch.iter(|| run(&b)));
    let q = scene_quiet();
    group.bench_function("192khz_60s_quiet", |bch| bch.iter(|| run(&q)));
    group.finish();
}

criterion_group!(benches, bench_comod);
criterion_main!(benches);
