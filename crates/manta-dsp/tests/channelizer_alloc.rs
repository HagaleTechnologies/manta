//! MAN-117: a steady-state `Channelizer::process` call allocates only its
//! outputs (each hop's `x` and `power`, and the returned `Vec`). MAN-527: a
//! steady-state `Channelizer::process_into` call with one `out` kept across
//! calls allocates nothing at all.

use manta_dsp::channelizer::Channelizer;
use num_complex::Complex32;

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

fn pseudo_noise(n: usize) -> Vec<Complex32> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    (0..n).map(|_| Complex32::new(next(), next())).collect()
}

#[test]
fn a_steady_state_process_call_allocates_only_its_outputs() {
    const FS: f64 = 192_000.0;
    const CHUNK: usize = 4096;
    let mut ch = Channelizer::new(FS, 14e6).unwrap();
    // About 2 s of warm-up in whole chunks: a partial last chunk would shift
    // the input window's fill pattern and cost the sliding buffer one more
    // amortised growth after warm-up, which is start-up, not per-hop, cost.
    let warm = (2.0 * FS) as usize / CHUNK * CHUNK;
    let iq = pseudo_noise(warm + 50 * CHUNK);
    for chunk in iq[..warm].chunks(CHUNK) {
        drop(ch.process(chunk));
    }
    for (i, chunk) in iq[warm..].chunks(CHUNK).enumerate() {
        let (out, events) = counting::events_during(|| ch.process(chunk));
        let hops = out.len() as u64;
        assert_eq!(hops, 8, "call {i}: 4096 samples at hop 512");
        assert!(
            events <= 2 * hops + 2,
            "call {i}: {events} allocation events for {hops} hops (bound {})",
            2 * hops + 2
        );
        drop(out);
    }
}

#[test]
fn process_into_allocates_nothing_after_warm_up() {
    for fs in [192_000.0, 96_000.0] {
        for chunk in [4096usize, 2048, 1000] {
            let mut ch = Channelizer::new(fs, 14e6).unwrap();
            let hop = ch.hop();
            let mut out = Vec::new();
            // Whole chunks only, as the test above explains.
            let warm = (2.0 * fs) as usize / chunk * chunk;
            let iq = pseudo_noise(warm + 1_000 * hop + chunk);
            for piece in iq[..warm].chunks(chunk) {
                ch.process_into(piece, &mut out);
            }
            let mut hops = 0usize;
            let mut total = 0u64;
            for piece in iq[warm..].chunks_exact(chunk) {
                let ((), events) = counting::events_during(|| ch.process_into(piece, &mut out));
                total += events;
                hops += out.len();
                if hops >= 1_000 {
                    break;
                }
            }
            assert!(hops >= 1_000, "fs {fs} chunk {chunk}: only {hops} hops");
            assert_eq!(
                total, 0,
                "fs {fs} chunk {chunk}: {total} allocation events over {hops} hops"
            );
        }
    }
}
