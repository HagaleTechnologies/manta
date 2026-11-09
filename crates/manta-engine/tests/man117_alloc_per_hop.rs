//! MAN-117: steady-state heap allocations per 512-sample hop, CPU time and
//! an output digest on the 300-signal `cpu_budget` scene. An `#[ignore]`d
//! probe, never a CI gate (CLAUDE.md: the CPU budget is a target). Uses only
//! APIs present on main at 7020ede, so the same file runs on a baseline tree.
//! Run in release:
//! `cargo test --release --locked -p manta-engine --test man117_alloc_per_hop -- --ignored --nocapture`
//!
//! This binary holds exactly one `#[test]`: the counting allocator below is
//! process-global (so rayon workers are counted), and a second test running
//! in parallel would pollute its counters.

use manta_dsp::channelizer::Channelizer;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

const FS: f64 = 192_000.0;
const CENTER_FREQ_HZ: f64 = 14_000_000.0;
/// Input samples per channelizer hop at 192 kS/s (N/4, N = 2048).
const HOP: usize = 512;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Forwards to `System` unchanged and counts allocation events and bytes.
struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged; the counters are side effects only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[derive(Clone, Copy)]
struct Snap {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
}

fn snap() -> Snap {
    Snap {
        allocs: ALLOCS.load(Ordering::Relaxed),
        reallocs: REALLOCS.load(Ordering::Relaxed),
        bytes: BYTES.load(Ordering::Relaxed),
    }
}

/// `(user + sys)` CPU seconds of this process, `getrusage(RUSAGE_SELF)`.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    // SAFETY: `rusage` is plain old data, and `getrusage` only writes it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a valid pointer to a live `rusage`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed");
    let to_secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0;
    to_secs(usage.ru_utime) + to_secs(usage.ru_stime)
}

#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    f64::NAN
}

/// FNV-1a 64, stable across toolchains (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The `cpu_budget` scene (`tests/cpu_budget.rs::cpu_budget_scene`): 300
/// looped 20 WPM signals at 15 dB across 14.000 MHz +/- 90 kHz, same seed.
fn scene(duration_s: f64) -> Vec<Complex32> {
    const N_SIGNALS: usize = 300;
    let signals: Vec<SignalSpec> = (0..N_SIGNALS)
        .map(|i| {
            let offset_hz = -90_000.0 + i as f64 * (180_000.0 / (N_SIGNALS - 1) as f64);
            SignalSpec {
                dit_units: 1.0,
                element_gap_units: 1.0,
                text: "CQ CQ DE K1BNC K1BNC K".into(),
                loop_text: true,
                wpm: 20.0,
                offset_hz,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
            }
        })
        .collect();
    let (samples, _texts) =
        render_scene(&signals, FS, duration_s, Some(0x4350_555F_4250_5431)).unwrap();
    samples
}

/// The channelizer alone: 2 s warm-up in 4096-sample chunks, then 10 s.
fn channelizer_line(iq: &[Complex32]) {
    let mut ch = Channelizer::new(FS, CENTER_FREQ_HZ).unwrap();
    let warm = (2.0 * FS) as usize;
    let timed = ((10.0 * FS) as usize).min(iq.len().saturating_sub(warm));
    for chunk in iq[..warm].chunks(4096) {
        std::hint::black_box(ch.process(chunk));
    }
    let mut hops = 0usize;
    let s0 = snap();
    for chunk in iq[warm..warm + timed].chunks(4096) {
        let out = ch.process(chunk);
        hops += out.len();
        std::hint::black_box(out);
    }
    let s1 = snap();
    let h = hops.max(1) as f64;
    println!(
        "MAN117 channelizer allocs_per_hop={:.3} reallocs_per_hop={:.3} bytes_per_hop={:.0} hops={hops}",
        (s1.allocs - s0.allocs) as f64 / h,
        (s1.reallocs - s0.reallocs) as f64 / h,
        (s1.bytes - s0.bytes) as f64 / h,
    );
}

#[test]
#[ignore = "allocation and CPU probe; run in release with --ignored --nocapture"]
fn steady_state_allocations_per_hop() {
    let secs: f64 = std::env::var("MAN117_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60.0);
    let iq = scene(secs);
    let half = iq.len() / 2;
    channelizer_line(&iq);

    for (name, digital) in [("default", true), ("cw_only", false)] {
        let mut cfg = PipelineConfig::default();
        cfg.detector.digital_window_filter = digital;
        let a0 = snap();
        let first = decode_samples(&iq[..half], FS, CENTER_FREQ_HZ, &cfg).unwrap();
        drop(first);
        let a1 = snap();
        let c1 = cpu_seconds();
        let report = decode_samples(&iq, FS, CENTER_FREQ_HZ, &cfg).unwrap();
        let c2 = cpu_seconds();
        let a2 = snap();
        // Differencing a half-length and a full-length decode cancels the
        // startup cost; what remains is `(len - half) / HOP` steady hops.
        let extra_hops = ((iq.len() - half) / HOP) as f64;
        let per = |full: u64, half_run: u64| (full as f64 - half_run as f64) / extra_hops;
        let allocs = per(a2.allocs - a1.allocs, a1.allocs - a0.allocs);
        let reallocs = per(a2.reallocs - a1.reallocs, a1.reallocs - a0.reallocs);
        let bytes = per(a2.bytes - a1.bytes, a1.bytes - a0.bytes);
        let full_cpu = c2 - c1;
        let digest =
            fnv1a(format!("{:?}|{:?}|{}", report.events, report.spots, report.text).as_bytes());
        println!(
            "MAN117 config={name} allocs_per_hop={allocs:.2} reallocs_per_hop={reallocs:.2} events_per_hop={:.2} bytes_per_hop={bytes:.0} full_cpu_s={full_cpu:.2} full_cpu_ratio={:.3} events={} spots={} digest={digest:016x}",
            allocs + reallocs,
            full_cpu / secs,
            report.events.len(),
            report.spots.len(),
        );
        assert!(!report.spots.is_empty(), "config {name} produced no spots");
    }
}
