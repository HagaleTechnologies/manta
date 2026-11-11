//! MAN-117: the caller-owned buffer forms `Demod::push_into`,
//! `TrackDecoder::push_hop_into` and `TrackDecoder::push_lead_in_hop_into`
//! return what their `Vec`-returning forms return, and a warmed
//! `push_into` with a reused buffer does not allocate.

use manta_decode::decoder::{DecodeConfig, Engine, TrackDecoder};
use manta_decode::envelope::{Demod, DemodConfig, Run};
use manta_decode::events::DecoderEvent;

/// Allocation events (allocs plus reallocs) on the calling thread while
/// counting is on. Other threads are not counted, so the other tests in
/// this binary, running in parallel, do not disturb it. Modelled on
/// `crates/manta-testkit/src/minimize.rs`'s `heap` module.
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static EVENTS: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Counting;

    fn note() {
        if ON.with(Cell::get) {
            EVENTS.with(|e| e.set(e.get() + 1));
        }
    }

    // SAFETY: every call forwards to `System` unchanged; `note` only
    // updates const-initialized thread-locals, which do not allocate.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Run `f`; return its result and the allocation events this thread
    /// made inside it.
    pub fn events_during<R>(f: impl FnOnce() -> R) -> (R, u64) {
        EVENTS.with(|e| e.set(0));
        ON.with(|o| o.set(true));
        let r = f();
        ON.with(|o| o.set(false));
        (r, EVENTS.with(Cell::get))
    }
}

#[global_allocator]
static COUNTING: counting::Counting = counting::Counting;

/// Hops per dit at 20 WPM: a 60 ms dit at the 375 Hz hop rate.
const HOPS_PER_DIT: f64 = 22.5;

/// A 20 WPM keyed envelope with LCG noise, deterministic, as
/// `(amplitude, sample_ts = i * 512)` pairs. The keying repeats "CQ " with
/// standard Morse gaps.
fn keyed_envelope(hops: usize) -> Vec<(f32, u64)> {
    // (mark?, length in dits) for "C", "Q", then a word gap.
    let mut pattern: Vec<(bool, u32)> = Vec::new();
    let letter = |elements: &[u32], gap_after: u32, p: &mut Vec<(bool, u32)>| {
        for (i, &e) in elements.iter().enumerate() {
            p.push((true, e));
            p.push((
                false,
                if i + 1 == elements.len() {
                    gap_after
                } else {
                    1
                },
            ));
        }
    };
    letter(&[3, 1, 3, 1], 3, &mut pattern); // C
    letter(&[3, 3, 1, 3], 7, &mut pattern); // Q, then a word gap
    let cycle_dits: u32 = pattern.iter().map(|&(_, d)| d).sum();
    let mut state: u64 = 0x5DEE_CE66_D1CE_4E5B;
    let mut noise = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 40) as f32 / (1u64 << 24) as f32) * 0.04
    };
    (0..hops)
        .map(|i| {
            let dit_pos = (i as f64 / HOPS_PER_DIT) % f64::from(cycle_dits);
            let mut acc = 0.0;
            let mut mark = false;
            for &(m, d) in &pattern {
                acc += f64::from(d);
                if dit_pos < acc {
                    mark = m;
                    break;
                }
            }
            let a = if mark { 1.0 } else { 0.05 };
            (a + noise(), i as u64 * 512)
        })
        .collect()
}

#[test]
fn push_into_returns_the_same_runs_as_push() {
    let env = keyed_envelope(6_000);
    let mut a = Demod::new(DemodConfig::default());
    let mut b = Demod::new(DemodConfig::default());
    let mut pushed: Vec<Run> = Vec::new();
    let mut appended: Vec<Run> = Vec::new();
    for &(amp, ts) in &env {
        pushed.extend(a.push(amp, ts));
        b.push_into(amp, ts, &mut appended);
    }
    assert!(pushed.len() > 100, "only {} runs", pushed.len());
    assert_eq!(pushed, appended);
}

#[test]
fn a_warmed_push_into_with_a_reused_buffer_does_not_allocate() {
    let env = keyed_envelope(4_000);
    let mut demod = Demod::new(DemodConfig::default());
    let mut out: Vec<Run> = Vec::new();
    for &(amp, ts) in &env[..2_000] {
        out.clear();
        demod.push_into(amp, ts, &mut out);
    }
    let (runs, events) = counting::events_during(|| {
        let mut runs = 0usize;
        for &(amp, ts) in &env[2_000..] {
            out.clear();
            demod.push_into(amp, ts, &mut out);
            runs += out.len();
        }
        runs
    });
    assert!(runs >= 20, "only {runs} runs completed");
    assert_eq!(
        events, 0,
        "{events} allocation events over 2,000 warmed hops"
    );
}

#[test]
fn push_hop_into_emits_the_same_events_as_push_hop_for_every_engine() {
    const LEAD_IN_HOPS: usize = 64;
    let env = keyed_envelope(6_000);
    let sentinel = DecoderEvent::SpeedUpdate {
        track_id: 9_999,
        wpm: -1.0,
    };
    for engine in [Engine::Legacy, Engine::EdgeLegacy, Engine::Hsmm] {
        let cfg = DecodeConfig {
            engine,
            ..DecodeConfig::default()
        };
        let mut returned = TrackDecoder::new(1, cfg.clone());
        let mut appending = TrackDecoder::new(1, cfg);
        let mut concatenated: Vec<DecoderEvent> = Vec::new();
        let mut buffer = vec![sentinel.clone()];
        for (i, &(amp, ts)) in env.iter().enumerate() {
            if i < LEAD_IN_HOPS {
                concatenated.extend(returned.push_lead_in_hop(amp, amp * amp, None, ts));
                appending.push_lead_in_hop_into(amp, amp * amp, None, ts, &mut buffer);
            } else {
                concatenated.extend(returned.push_hop(amp, amp * amp, None, ts));
                appending.push_hop_into(amp, amp * amp, None, ts, &mut buffer);
            }
        }
        assert_eq!(buffer[0], sentinel, "{engine:?}: the sentinel moved");
        assert_eq!(buffer[1..], concatenated[..], "{engine:?}");
        assert!(!concatenated.is_empty(), "{engine:?}: no events at all");
        if engine == Engine::Legacy {
            assert!(
                concatenated
                    .iter()
                    .any(|e| matches!(e, DecoderEvent::CharDecoded { .. })),
                "Legacy decoded no characters"
            );
        }
    }
}
