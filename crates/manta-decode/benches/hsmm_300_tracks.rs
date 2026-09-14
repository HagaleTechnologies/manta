//! SPEC v2 §8.4 CPU budget: 300 tracks x 10 s of keying decoded through the
//! `Hsmm` engine must complete in <= 2.5 s wall on an M-series core (<= 25%
//! of real time). Task 11 measures and records this number; it is not this
//! task's job to make it pass (Task 12 is the stage-2 gate).
//!
//! **Scope caveat (Codex review, PR #161):** the stated budget is for the
//! full pipeline at 192 kS/s, including the PFB channelizer and detector --
//! this bench measures `TrackDecoder` alone, fed prebuilt scalar envelopes,
//! bypassing channelization/detection entirely. The measured 4.25x-over
//! result (`docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md` §6) is
//! therefore a LOWER bound on the true budget overage, not the full-pipeline
//! number the spec describes -- a full-pipeline 300-simultaneous-track
//! bench at 192 kS/s is real, substantial follow-up work (synthesizing a
//! realistic multi-signal scene and running it through `Channelizer` +
//! the detector, not just `TrackDecoder`), tracked on MAN-171 rather than
//! attempted here.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use manta_decode::decoder::{DecodeConfig, Engine, TrackDecoder};

fn envelope(hops: usize) -> Vec<f32> {
    // 35 WPM "CQ TEST W5AU" at 40 dB depth with 4-hop ramps, repeated to `hops`.
    let pat = [1.0f32; 13]
        .iter()
        .chain([0.01f32; 13].iter())
        .chain([1.0f32; 39].iter())
        .chain([0.01f32; 39].iter())
        .copied()
        .collect::<Vec<_>>();
    (0..hops).map(|i| pat[i % pat.len()]).collect()
}

fn bench(c: &mut Criterion) {
    let env = envelope(375 * 10); // 10 s
    let mut g = c.benchmark_group("hsmm");
    g.throughput(Throughput::Elements((300 * env.len()) as u64));
    g.bench_function("300_tracks_10s", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for id in 0..300u32 {
                // Avoid clippy::field_reassign_with_default (this branch has
                // hit it repeatedly with the `let mut cfg = ...; cfg.engine
                // = ...;` pattern) by constructing the non-default field
                // directly.
                let cfg = DecodeConfig {
                    engine: Engine::Hsmm,
                    ..Default::default()
                };
                let mut d = TrackDecoder::new(id, cfg);
                for (i, &a) in env.iter().enumerate() {
                    total += d.push_envelope(a, i as u64 * 512).len();
                }
            }
            total
        })
    });
    g.finish();
}
criterion_group!(benches, bench);
criterion_main!(benches);
