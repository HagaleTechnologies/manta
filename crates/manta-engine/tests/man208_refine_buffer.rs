//! MAN-208 Phase 3: the refinement retention buffer and the admission
//! trigger predicates, at the public `manta_engine::refine` boundary.

use manta_decode::decoder::RefineConfig;
use manta_engine::refine::{
    ambiguity_trigger, separability_trigger, speed_trigger, RefineBuffers, REPLAY_PREROLL_HOPS,
};
use manta_spot::RefineCandidate;

fn cfg(lookback_secs: f64) -> RefineConfig {
    RefineConfig {
        enabled: true,
        lookback_secs,
        ..RefineConfig::default()
    }
}

fn entry(ts: u64) -> (f32, f32, Option<f32>, u64, f32, f64) {
    (1.0, 1.0, None, ts, 20.0, 14_000_000.0)
}

#[test]
fn buffer_retains_only_the_lookback_window() {
    // 4 s at 375 hops/s = 1500 hops; FIFO at capacity, in hops.
    let mut buffers = RefineBuffers::new(&cfg(4.0));
    for ts in 0..2000u64 {
        buffers.record(1, entry(ts * 256));
    }
    assert_eq!(buffers.len(), 1500);
    let slice = buffers.replay_slice(1, (1999 * 256, 1999 * 256)).unwrap();
    // Pre-roll: REPLAY_PREROLL_HOPS before the span, still fully retained.
    assert_eq!(
        slice.first().unwrap().3,
        (1999 - REPLAY_PREROLL_HOPS as u64) * 256
    );
}

#[test]
fn buffer_breaks_at_input_gap_and_never_replays_across_it() {
    let mut buffers = RefineBuffers::new(&cfg(10.0));
    for ts in 0..100u64 {
        buffers.record(1, entry(ts * 256));
    }
    buffers.break_at_input_gap();
    assert_eq!(buffers.len(), 0, "the gap invalidates every retained hop");
    assert!(
        buffers.replay_slice(1, (0, 99 * 256)).is_none(),
        "a pre-gap span is never replayed"
    );
    for ts in 100..200u64 {
        buffers.record(1, entry(ts * 256));
    }
    assert!(
        buffers.replay_slice(1, (0, 99 * 256)).is_none(),
        "post-gap hops are not contiguous with a pre-gap span"
    );
    assert!(
        buffers.replay_slice(1, (100 * 256, 199 * 256)).is_some(),
        "a fully post-gap span replays"
    );
}

#[test]
fn buffer_is_dropped_with_its_track() {
    let mut buffers = RefineBuffers::new(&cfg(10.0));
    for ts in 0..100u64 {
        buffers.record(7, entry(ts * 256));
    }
    buffers.drop_track(7);
    assert!(buffers.is_empty());
    assert!(buffers.replay_slice(7, (0, 99 * 256)).is_none());
}

#[test]
fn byte_bound_is_enforced_deterministically() {
    // The global bound evicts the oldest (track, hop) -- smallest front
    // sample_ts, ties by smallest track_id -- never wall clock.
    let mut buffers = RefineBuffers::new(&cfg(10.0)).with_entry_budget(4);
    // Track 2's front hop is the oldest of the two tracks' second entries.
    for ts in 0..3u64 {
        buffers.record(1, entry(1_000 + ts));
    }
    for ts in 0..3u64 {
        buffers.record(2, entry(500 + ts));
    }
    assert_eq!(buffers.len(), 4);
    assert_eq!(buffers.evicted_entries(), 2);
    // Track 2 (oldest front) lost its two oldest hops; track 1 kept all 3
    // ... no: the bound evicts globally until 4 remain, so track 1 also
    // lost its oldest. What must hold is the ORDER: every surviving entry
    // is newer than every evicted one.
    let survivors: Vec<u64> = [
        buffers.replay_slice(1, (1000, 1002)),
        buffers.replay_slice(2, (502, 502)),
    ]
    .into_iter()
    .flatten()
    .flatten()
    .map(|e| e.3)
    .collect();
    assert_eq!(survivors, vec![1000, 1001, 1002, 502]);
    // 500 and 501 (track 2) were the two oldest overall and were evicted
    // first, in sample_ts order.
}

#[test]
fn a_span_without_preroll_still_replays_whole() {
    let mut buffers = RefineBuffers::new(&cfg(10.0));
    for ts in 0..(REPLAY_PREROLL_HOPS + 100) as u64 {
        buffers.record(1, entry(ts * 256));
    }
    let span = (50 * 256, 99 * 256);
    let slice = buffers.replay_slice(1, span).unwrap();
    assert_eq!(slice.last().unwrap().3, 99 * 256);
    assert_eq!(
        slice.first().unwrap().3,
        0,
        "the pre-roll reaches back through everything retained"
    );
}

#[test]
fn the_global_bound_keeps_the_newest_entries_across_300_tracks() {
    // MAN-525: recording is (sample_ts, track_id) order, so oldest-front
    // eviction is FIFO over the record stream: the last 174_762 records
    // survive.
    let mut b = RefineBuffers::new(&cfg(10.0));
    for hop in 0..3800u64 {
        for id in 0..300u32 {
            b.record(id, entry(hop * 256));
        }
    }
    assert_eq!(b.len(), 174_762);
    assert_eq!(b.evicted_entries(), 300 * 3800 - 174_762);
    for id in 0..300u32 {
        assert!(b.replay_slice(id, (3218 * 256, 3799 * 256)).is_some());
        assert_eq!(
            b.replay_slice(id, (3217 * 256, 3799 * 256)).is_some(),
            id >= 138
        );
    }
}

#[test]
fn equal_front_timestamps_evict_the_smallest_track_id_first() {
    let mut b = RefineBuffers::new(&cfg(10.0)).with_entry_budget(3);
    for id in [5, 2, 9] {
        b.record(id, entry(100));
    }
    b.record(7, entry(200)); // evicts (100, 2)
    b.record(3, entry(300)); // evicts (100, 5)
    assert_eq!(b.evicted_entries(), 2);
    assert!(b.replay_slice(2, (100, 100)).is_none());
    assert!(b.replay_slice(5, (100, 100)).is_none());
    assert!(b.replay_slice(9, (100, 100)).is_some());
    assert!(b.replay_slice(7, (200, 200)).is_some());
    assert!(b.replay_slice(3, (300, 300)).is_some());
}

fn candidate(rival: Option<&str>, alternatives: usize) -> RefineCandidate<'_> {
    RefineCandidate {
        episode_wpm: 20.0,
        neighbor_gap_hz: f64::INFINITY,
        latest_word_seq: 1,
        track_id: 1,
        word_seq: 0,
        call: "K3ABC",
        span: (0, 100),
        reps: 1,
        wpm: 20.0,
        rival,
        alternatives,
        wpm_stable: true,
    }
}

#[test]
fn ambiguity_trigger_fires_on_rival_withheld_word() {
    assert!(ambiguity_trigger(&candidate(Some("K3ABD"), 0)));
    assert!(ambiguity_trigger(&candidate(None, 1)));
    assert!(!ambiguity_trigger(&candidate(None, 0)));
}

#[test]
fn ambiguity_trigger_ignores_decided_episodes() {
    // A decided episode: no rival, no unresolved alternatives, its
    // repetition requirement met.
    let decided = RefineCandidate {
        reps: 2,
        ..candidate(None, 0)
    };
    assert!(!ambiguity_trigger(&decided));
}

#[test]
fn speed_trigger_fires_only_on_a_material_change() {
    let at = |wpm: f32| RefineCandidate {
        wpm,
        ..candidate(None, 0)
    };
    assert!(!speed_trigger(&at(22.0), 20.0), "under the 3 WPM bar");
    assert!(speed_trigger(&at(25.0), 20.0));
    assert!(speed_trigger(&at(15.0), 20.0));
}

/// Validate round 2 (MAN-208): the plan's speed trigger is a material
/// change "and stable" -- a new RELIABLE estimate. A large jump the
/// decoder has not yet held across later words is a transient, not a
/// reason to spend replay budget.
#[test]
fn speed_trigger_requires_a_stable_new_estimate() {
    let transient = RefineCandidate {
        wpm: 28.0,
        wpm_stable: false,
        ..candidate(None, 0)
    };
    assert!(!speed_trigger(&transient, 20.0), "unstable jump admitted");
    let held = RefineCandidate {
        wpm_stable: true,
        ..transient
    };
    assert!(speed_trigger(&held, 20.0));
}

#[test]
fn separability_trigger_fires_when_a_close_pair_separates() {
    assert!(!separability_trigger(120.0, 140.0), "never close");
    assert!(!separability_trigger(50.0, 80.0), "still close");
    assert!(separability_trigger(50.0, 93.75));
}

#[test]
fn no_trigger_outside_the_lookback_window() {
    // The window filter is the validator query's (`refine_candidates`
    // drops words older than the lookback), pinned end to end in
    // `man208_late_rescue.rs`; here the retention side: an evicted span is
    // a buffer miss, never a partial replay.
    let mut buffers = RefineBuffers::new(&cfg(4.0));
    for ts in 0..2000u64 {
        buffers.record(1, entry(ts * 256));
    }
    assert!(buffers.replay_slice(1, (0, 256)).is_none());
    assert!(buffers.replay_slice(1, (1990 * 256, 1999 * 256)).is_some());
}

#[test]
fn admission_is_stable_in_track_word_order() {
    // `RefineBuffers` iterates tracks in BTreeMap order, so both eviction
    // and slicing are deterministic in `(track_id, hop)`; the scheduler's
    // admission order comes from `refine_candidates`'s same-key iteration
    // and is pinned by the rescue tests.
    let mut buffers = RefineBuffers::new(&cfg(10.0)).with_entry_budget(2);
    buffers.record(9, entry(300));
    buffers.record(2, entry(200));
    buffers.record(5, entry(100));
    assert_eq!(buffers.evicted_entries(), 1);
    // The globally oldest hop (track 5's own ts=100) was the one evicted.
    assert!(buffers.replay_slice(5, (100, 100)).is_none());
    assert!(buffers.replay_slice(9, (300, 300)).is_some());
    assert!(buffers.replay_slice(2, (200, 200)).is_some());
}
