//! MAN-193: `Evidence` alone, 300 tracks x 3,750 hops (10 s) each, pushed
//! and fully flushed through the public methods. The fixed cases hold the
//! hold window at h = 60 (the default `u_init`) and h = 224 (the default
//! slow-speed limit) so width is the only changed factor; the stress case
//! alternates h between 223 and 225 after every pushed hop, which retreats
//! a window bound on every center and forces the cached maximum to rebuild
//! each time (`docs/DECISIONS/2026-10-14-man193-centered-maximum-deque.md`).
//!
//! Scope: the evidence front end only. It excludes channelization,
//! detection and the HSMM search (`hsmm_300_tracks` measures the decoder).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use manta_decode::evidence::{Evidence, EvidenceConfig};
use std::hint::black_box;

const TRACKS: usize = 300;
const HOPS: usize = 375 * 10;

/// One deterministic amplitude stream per track: a keyed 35 WPM-like
/// pattern at 40 dB depth with a per-track phase and slow ripple.
fn inputs() -> Vec<Vec<f32>> {
    (0..TRACKS)
        .map(|t| {
            (0..HOPS)
                .map(|i| {
                    let k = (i + 7 * t) % 104;
                    let on = k < 13 || (26..65).contains(&k);
                    let ripple = 1.0 + 0.1 * ((i as f32) * 0.011 + t as f32).sin();
                    if on {
                        ripple
                    } else {
                        0.01 * ripple
                    }
                })
                .collect()
        })
        .collect()
}

/// `u_init_hops` such that h = round(4 * u) is the requested width.
fn cfg(u_init_hops: f32) -> EvidenceConfig {
    EvidenceConfig {
        u_init_hops,
        ..EvidenceConfig::default()
    }
}

/// Runs every track and returns (emitted hops, checksum of every emitted
/// hop's mark level and LLR). `swing` alternates `set_u_ref` after every
/// pushed hop.
fn run(inputs: &[Vec<f32>], u: f32, swing: Option<[f32; 2]>) -> (usize, f64) {
    let mut emitted = 0usize;
    let mut checksum = 0.0f64;
    for env in inputs {
        let mut e = Evidence::new(cfg(u));
        for (i, &a) in env.iter().enumerate() {
            for ev in e.push(a, 0.005, i as u64 * 512) {
                emitted += 1;
                checksum += (ev.mark_level + ev.llr) as f64;
            }
            if let Some(s) = swing {
                e.set_u_ref(s[i % 2]);
            }
        }
        for ev in e.flush() {
            emitted += 1;
            checksum += (ev.mark_level + ev.llr) as f64;
        }
    }
    (emitted, checksum)
}

fn bench(c: &mut Criterion) {
    let inputs = inputs();
    let cases: [(&str, f32, Option<[f32; 2]>); 3] = [
        ("fixed_h60", 15.0, None),
        ("fixed_h224", 56.0, None),
        ("resize_stress_h223_h225", 56.0, Some([55.75, 56.25])),
    ];
    // Every case emits exactly one hop per input; check it outside the
    // timed loop.
    for &(name, u, swing) in &cases {
        let (emitted, checksum) = run(&inputs, u, swing);
        assert_eq!(emitted, TRACKS * HOPS, "{name}");
        println!("evidence/{name}: emitted {emitted} checksum {checksum:.6}");
    }
    let mut g = c.benchmark_group("evidence");
    g.sample_size(10);
    g.throughput(Throughput::Elements((TRACKS * HOPS) as u64));
    for (name, u, swing) in cases {
        g.bench_function(format!("300_tracks_10s_{name}"), |b| {
            b.iter(|| run(black_box(&inputs), black_box(u), swing))
        });
    }
    g.finish();
}
criterion_group!(benches, bench);
criterion_main!(benches);
