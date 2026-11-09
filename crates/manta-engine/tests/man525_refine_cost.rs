//! MAN-525: the cost of recording one refinement hop by track count. An
//! `#[ignore]`d probe, never a CI timing gate (CLAUDE.md: the CPU budget is
//! a target, not a gate). Run it in release:
//! `cargo test --release -p manta-engine --locked --test man525_refine_cost -- --ignored --nocapture`.

use manta_decode::decoder::RefineConfig;
use manta_engine::refine::RefineBuffers;
use std::time::Instant;

fn entry(ts: u64) -> (f32, f32, Option<f32>, u64, f32, f64) {
    (1.0, 1.0, None, ts, 20.0, 14_000_000.0)
}

#[test]
#[ignore = "timing probe; run in release with --ignored --nocapture"]
fn record_cost_by_track_count() {
    let cfg = RefineConfig {
        enabled: true,
        ..RefineConfig::default()
    };
    for tracks in [1u32, 46, 47, 100, 300, 600] {
        let mut b = RefineBuffers::new(&cfg);
        // Warm-up: past both the per-track lookback (3750 hops at 10 s) and,
        // from 47 tracks, the 174_762-entry global bound.
        let warm_hops = 4_000u64;
        for hop in 0..warm_hops {
            for id in 0..tracks {
                b.record(id, entry(hop * 256));
            }
        }
        let evicted_before = b.evicted_entries();
        let timed_hops = (300_000 / u64::from(tracks)).max(1);
        let start = Instant::now();
        for hop in warm_hops..warm_hops + timed_hops {
            for id in 0..tracks {
                b.record(id, entry(hop * 256));
            }
        }
        let elapsed = start.elapsed();
        let records = timed_hops * u64::from(tracks);
        let retained_secs = b.len() as f64 / f64::from(tracks) / 375.0;
        println!(
            "MAN525 tracks={tracks} ns_per_record={:.1} retained_secs_per_track={retained_secs:.2} evictions_in_window={}",
            elapsed.as_nanos() as f64 / records as f64,
            b.evicted_entries() - evicted_before
        );
        if tracks >= 47 {
            assert_eq!(b.len(), 174_762);
        } else {
            assert_eq!(b.len(), tracks as usize * 3_750);
        }
    }
}
