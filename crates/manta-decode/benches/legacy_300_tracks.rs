//! MAN-544: per-track decode CPU of the legacy engine, the cost gate of the
//! speed/timing hypothesis bank (plan D13). 300 legacy `TrackDecoder`s, each
//! fed 20 s of a deterministic 375 Hz scalar envelope through
//! `push_envelope` and then `finish`, single thread, `Demod` included.
//!
//! Scenes (generated inline with a xorshift, no testkit dependency):
//! - `steady`: `CQ CQ DE K1ABC K1ABC K` looped, 10-24 hops per dit by
//!   track, +/-0.025 uniform noise on a 1.0 / 0.01 keyed level;
//! - `steps`: the same with a speed step at 10 s, even tracks twice as fast,
//!   odd tracks half as fast;
//! - `noisy`: `steady` under a 0.5 Hz `0.55 + 0.45 sin` fade, with a 10 ms
//!   (4 hop) dropout in the middle of every 31st mark.
//!
//! Each scene runs `default` (`DecodeConfig::default()`), `width1` (the
//! bank off) and `width3_observe` (three hypotheses scored, never switched:
//! the bank's full scoring cost with main's decode).
//!
//! Scope: `TrackDecoder` only, fed prebuilt envelopes; channelization and
//! detection are excluded (`hsmm_300_tracks` has the same caveat). It is a
//! measurement, not a CI gate (CLAUDE.md: the CPU budget is "a target, not a
//! CI gate"). Compare a change against a saved baseline:
//! `cargo bench -p manta-decode --bench legacy_300_tracks -- --save-baseline main`
//! on the base tree, then `-- --baseline main` on the change.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::timing::SpeedBankConfig;
use manta_decode::tree::pattern_for;
use std::hint::black_box;

const TRACKS: usize = 300;
const HOP_HZ: usize = 375;
const SECONDS: usize = 20;
const HOPS: usize = HOP_HZ * SECONDS;
const TEXT: &str = "CQ CQ DE K1ABC K1ABC K";

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in [-1, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// `TEXT` as (on, dit units) segments, a 7-unit word gap after each loop.
fn units() -> Vec<(bool, usize)> {
    let mut segs = Vec::new();
    for (wi, word) in TEXT.split(' ').enumerate() {
        if wi > 0 {
            segs.push((false, 7));
        }
        for (ci, c) in word.chars().enumerate() {
            if ci > 0 {
                segs.push((false, 3));
            }
            let pat = pattern_for(c).expect("TEXT is keyable");
            for (ei, e) in pat.chars().enumerate() {
                if ei > 0 {
                    segs.push((false, 1));
                }
                segs.push((true, if e == '-' { 3 } else { 1 }));
            }
        }
    }
    segs.push((false, 7));
    segs
}

/// One track's keyed (on/off) hop stream: `dit_before` hops per dit up to
/// `step_hop`, `dit_after` from there on.
fn keyed(dit_before: usize, dit_after: usize, step_hop: usize) -> Vec<(bool, usize)> {
    let segs = units();
    let mut out = Vec::with_capacity(HOPS);
    let mut mark_index = 0usize;
    'outer: loop {
        for &(on, n) in &segs {
            let dit = if out.len() < step_hop {
                dit_before
            } else {
                dit_after
            };
            for _ in 0..n * dit {
                if out.len() == HOPS {
                    break 'outer;
                }
                out.push((on, mark_index));
            }
            if on {
                mark_index += 1;
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Scene {
    Steady,
    Steps,
    Noisy,
}

fn envelopes(scene: Scene) -> Vec<Vec<f32>> {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    (0..TRACKS)
        .map(|t| {
            let dit = 10 + t % 15;
            let (after, step_hop) = match scene {
                Scene::Steps if t % 2 == 0 => (dit.div_ceil(2), HOPS / 2),
                Scene::Steps => (dit * 2, HOPS / 2),
                _ => (dit, HOPS),
            };
            let stream = keyed(dit, after, step_hop);
            let mut mark_start = 0usize;
            let mut prev_on = false;
            stream
                .iter()
                .enumerate()
                .map(|(i, &(on, mark))| {
                    if on && !prev_on {
                        mark_start = i;
                    }
                    prev_on = on;
                    let mut level = if on { 1.0f32 } else { 0.01 };
                    if let Scene::Noisy = scene {
                        let ts = i as f32 / HOP_HZ as f32;
                        level *= 0.55 + 0.45 * (std::f32::consts::TAU * 0.5 * ts).sin();
                        let mid = mark_start + after.min(dit) / 2;
                        if on && mark % 31 == 30 && (mid..mid + 4).contains(&i) {
                            level = 0.01;
                        }
                    }
                    (level + 0.025 * rng.unit()).max(0.0)
                })
                .collect()
        })
        .collect()
}

fn decode_all(envs: &[Vec<f32>], cfg: DecodeConfig) -> usize {
    let mut total = 0usize;
    for (id, env) in envs.iter().enumerate() {
        let mut d = TrackDecoder::new(id as u32, cfg.clone());
        for (i, &a) in env.iter().enumerate() {
            total += d.push_envelope(a, i as u64 * 512).len();
        }
        total += d.finish().len();
    }
    total
}

fn bench(c: &mut Criterion) {
    for (name, scene) in [
        ("steady", Scene::Steady),
        ("steps", Scene::Steps),
        ("noisy", Scene::Noisy),
    ] {
        let envs = envelopes(scene);
        let mut g = c.benchmark_group(format!("legacy_300_tracks/{name}"));
        g.sample_size(10);
        g.throughput(Throughput::Elements((TRACKS * HOPS) as u64));
        g.bench_function("default", |b| {
            b.iter(|| decode_all(black_box(&envs), DecodeConfig::default()))
        });
        for (variant, width, switching) in [("width1", 1, false), ("width3_observe", 3, false)] {
            let cfg = DecodeConfig {
                speed_bank: SpeedBankConfig { width, switching },
                ..Default::default()
            };
            g.bench_function(variant, |b| {
                b.iter(|| decode_all(black_box(&envs), cfg.clone()))
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
