//! MAN-186: cost of the `Decimator` (cascaded `HalfbandStage`s) per second
//! of input IQ, at the factor/rate pairs the variable-width capture path
//! (`--capture-rate-hz`, `manta-input::DecimatingSource`) builds: 192 kS/s
//! down to 96, 48 and 24 kS/s (factors 2, 4, 8), 96 kS/s down to 48 and
//! 24 kS/s (factors 2, 4), and 48 kHz rig audio down to 6 kS/s (factor 8).
//! Each pair runs twice: one whole second of input per `process` call, and
//! the live shape -- `RAW_CHUNK_SAMPLES (4096) * factor` input samples per
//! call, which is what `DecimatingSource::read` feeds the decimator. Steady
//! state: the decimator is built once and fed repeatedly, so construction
//! (the Kaiser design) is not in the timed loop. Throughput is input
//! samples, so criterion's "elem/s" divided by the pair's `fs_in` is the
//! fraction of one real-time stream the stage keeps up with. Not gated in
//! CI; host numbers only (no Pi4 claim). The decision record quotes time
//! per 1 s of input as the fraction of one host core.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use manta_dsp::decimate::Decimator;
use num_complex::Complex32;
use std::hint::black_box;
use std::time::Duration;

/// `manta-input::decimate::RAW_CHUNK_SAMPLES`: decimated samples per
/// `DecimatingSource` read; the raw read is this times the factor.
const RAW_CHUNK_SAMPLES: usize = 4096;

/// (fs_in Hz, factor, label)
const CASES: [(usize, usize, &str); 6] = [
    (192_000, 2, "192k_to_96k_x2"),
    (192_000, 4, "192k_to_48k_x4"),
    (192_000, 8, "192k_to_24k_x8"),
    (96_000, 2, "96k_to_48k_x2"),
    (96_000, 4, "96k_to_24k_x4"),
    (48_000, 8, "48k_to_6k_x8"),
];

/// Deterministic LCG (same shape as `comod.rs`).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    /// Uniform in [-0.5, 0.5).
    fn centered(&mut self) -> f32 {
        self.next() as f32 / (1u64 << 31) as f32 - 0.5
    }
}

/// One second of input at `fs`: three in-band tones plus LCG noise, so the
/// samples are dense and non-trivial (no denormal or zero fast paths).
fn input(fs: usize) -> Vec<Complex32> {
    let mut rng = Lcg(0x4d41_4e31_3836_u64);
    (0..fs)
        .map(|i| {
            let t = i as f64 / fs as f64;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (f, a) in [(2_000.0, 0.3), (-7_500.0, 0.2), (12_340.0, 0.1)] {
                let phi = 2.0 * std::f64::consts::PI * f * t;
                re += a * phi.cos();
                im += a * phi.sin();
            }
            Complex32::new(
                re as f32 + 0.05 * rng.centered(),
                im as f32 + 0.05 * rng.centered(),
            )
        })
        .collect()
}

fn bench_decimate(c: &mut Criterion) {
    let mut group = c.benchmark_group("decimate");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for (fs_in, factor, label) in CASES {
        let x = input(fs_in);
        group.throughput(Throughput::Elements(x.len() as u64));

        // Outside the timed loop: a fresh decimator keeps exactly
        // `fs_in / factor` of one second (parity starts at 0), so a change
        // that drops or doubles outputs fails the bench instead of
        // quietly changing what it times.
        let mut check = Decimator::new(fs_in as f64, factor).unwrap();
        assert_eq!(check.process(&x).len(), fs_in / factor, "{label}");

        let mut whole = Decimator::new(fs_in as f64, factor).unwrap();
        group.bench_function(format!("{label}_whole_1s"), |b| {
            b.iter(|| black_box(whole.process(black_box(&x))))
        });

        let mut live = Decimator::new(fs_in as f64, factor).unwrap();
        let chunk = RAW_CHUNK_SAMPLES * factor;
        group.bench_function(format!("{label}_live_chunks"), |b| {
            b.iter(|| {
                let mut kept = 0usize;
                for ch in x.chunks(chunk) {
                    kept += black_box(live.process(black_box(ch))).len();
                }
                kept
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_decimate);
criterion_main!(benches);
