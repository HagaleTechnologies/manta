//! ROADMAP.md M2 CPU-budget accept criterion (see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md and
//! benches/cpu_budget.rs's module doc). This test's own `assert!`s check
//! the Mac budget (< 0.5x of one core) against both wall-clock and
//! (user+sys) CPU-time ratios -- see 2026-09-02 pins for why both, not
//! just wall-clock. Pi4's budget is a different number (< 1.0x) that a
//! human judges from the printed CPU-time line on real hardware, per
//! docs/RUNBOOKS/m2-pi4-cpu-budget.md -- that manual Pi4 run is an
//! explicitly flagged outstanding step, same pattern as M1's still-
//! outstanding W1AW live-copy run (see CLAUDE.md Status).

use manta_decode::events::DecoderEvent;
use manta_engine::PipelineConfig;
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::collections::HashMap;

/// `(user + sys)` CPU seconds consumed by this process so far, per
/// `getrusage(RUSAGE_SELF)`. Wall clock alone can't distinguish "cheap,
/// truly single-core work" from "expensive work spread thin across many
/// threads" -- see docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md.
/// The Pi4 accept criterion (< 1 full core, ROADMAP.md M2) is a CPU-time
/// budget, not a wall-clock one; this is what actually answers it.
///
/// Unix-only: `getrusage`/`RUSAGE_SELF` aren't in Windows' `libc` surface,
/// and this repo is explicitly cross-platform (AGENTS.md) -- the Mac/Pi4
/// gate this test measures is inherently Unix-hardware-only anyway (there's
/// no Windows leg of this ROADMAP criterion), so the Windows build just
/// skips the CPU-time checks below rather than pulling in an extra crate
/// (e.g. `windows-sys`'s `GetProcessTimes`) for a number nothing needs.
#[cfg(unix)]
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage failed");
    let to_secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0;
    to_secs(usage.ru_utime) + to_secs(usage.ru_stime)
}

#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    f64::NAN
}

fn track_id(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. } => *track_id,
    }
}

/// `sample_ts` for the two `DecoderEvent` variants that carry one --
/// `SpeedUpdate`/`TrackMeta` don't, so `None` for those.
fn event_sample_ts(e: &DecoderEvent) -> Option<u64> {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. } => Some(*sample_ts),
        DecoderEvent::SpeedUpdate { .. } | DecoderEvent::TrackMeta { .. } => None,
    }
}

/// Count of tracks that were producing output continuously across nearly
/// the whole file -- a proxy for "concurrently active for nearly the whole
/// run", not exact instantaneous concurrency. `decode_samples`'s public
/// event stream has no track-opened/track-closed events, only per-
/// character/word timestamps, so there's no way to reconstruct the true
/// simultaneous-ACTIVE-track count from outside `TrackManager` without
/// adding one (a `manta-engine`/`manta-dsp` production API change, out of
/// scope for this measurement-only ticket -- see the scope note in
/// docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md).
///
/// Two checks, both against a track's *own* sorted event timestamps:
/// 1. First event within the opening 10%, last event within the closing
///    10% -- catches a track promoted late or evicted early.
/// 2. No gap between consecutive events wider than `MAX_GAP_S` -- catches
///    the case bounds-checking alone misses: a track present near both
///    ends of the file but silent (closed and never really "sustained")
///    for a stretch in the middle. At 20 WPM with this scene's continuous
///    `loop_text` CQ loop (no fading/silence), a genuinely continuous
///    track's `CharDecoded`/`WordBoundary` events land every well under a
///    second; `MAX_GAP_S` is set far more generous than that (but well
///    under the ~30s GC silent-timer in `track.rs` that would close and
///    reissue a fresh `track_id` for a real silence) specifically so this
///    catches an internal gap, not normal per-character/word jitter.
///
/// Materially stronger than counting raw distinct `track_id`s (which a
/// churning detector could inflate past 300 without ever holding anywhere
/// near 300 open at once) or bounds-only checking (which a track that
/// churned off only in the middle could still pass), though still not
/// proof of exact simultaneity.
fn sustained_track_count(
    events: &[DecoderEvent],
    steady_state_start: u64,
    total_samples: u64,
    fs: f64,
) -> usize {
    const MAX_GAP_S: f64 = 3.0;

    let mut timestamps: HashMap<u32, Vec<u64>> = HashMap::new();
    for e in events {
        let Some(ts) = event_sample_ts(e) else {
            continue;
        };
        timestamps.entry(track_id(e)).or_default().push(ts);
    }

    let max_gap_samples = (MAX_GAP_S * fs) as u64;

    // Bracket the true post-warmup interval with two virtual endpoints
    // (steady_state_start, total_samples) and require the SAME max-gap
    // bound across every gap, including the edges -- not just between a
    // track's own recorded events. A percentage-of-file margin (10%/90%,
    // this function's prior version) left real slack at both ends that
    // the internal-gap check alone didn't cover: a track could be silent
    // from steady_state_start up to its first event, or from its last
    // event to the end of file, without either gap ever being checked
    // (Codex review, this PR -- ~17% of the interval could go
    // unaccounted-for at the file's own 60s/58s scale). Folding the
    // boundaries into the same windowed gap check closes that instead of
    // just tightening another percentage.
    timestamps
        .values()
        .filter(|ts| {
            let mut sorted = (*ts).clone();
            sorted.push(steady_state_start);
            sorted.push(total_samples);
            sorted.sort_unstable();
            sorted.windows(2).all(|w| w[1] - w[0] <= max_gap_samples)
        })
        .count()
}

fn cpu_budget_scene() -> (Vec<Complex32>, f64, f64, PipelineConfig) {
    const FS: f64 = 192_000.0;
    const CENTER_FREQ_HZ: f64 = 14_000_000.0;
    // 60s, not the original 15s: the 2s detector warmup isn't the only cost
    // excluded from steady_state_s's denominator below -- the channelizer/
    // PFB decomposition runs at a constant per-sample rate for the WHOLE
    // call, warmup included, so "subtract warmup duration from the
    // denominator" alone still leaves 2s of real (non-decoder-pool, but
    // non-zero) channelizer cost sitting in the numerator, which a short
    // scene can't average away (Codex review, this PR: could turn a real
    // sub-budget result into a false failure). Lengthening the scene
    // shrinks warmup's share of the total from 13% (2s of 15s) to ~3.3%
    // (2s of 60s), shrinking that residual error proportionally without
    // needing a new engine API to time only the post-warmup portion of a
    // single `decode_samples` call (out of scope -- see the pins doc).
    const DURATION_S: f64 = 60.0;
    const N_SIGNALS: usize = 300;

    let signals: Vec<SignalSpec> = (0..N_SIGNALS)
        .map(|i| {
            let offset_hz = -90_000.0 + i as f64 * (180_000.0 / (N_SIGNALS - 1) as f64);
            SignalSpec {
                text: "CQ CQ DE K1BNC K1BNC K".into(),
                loop_text: true,
                wpm: 20.0,
                offset_hz,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            }
        })
        .collect();
    let (samples, _texts) =
        render_scene(&signals, FS, DURATION_S, Some(0x4350_555F_4250_5431)).unwrap();
    (samples, FS, CENTER_FREQ_HZ, PipelineConfig::default())
}

/// This test MUST be run with `cargo test --release` for a meaningful result.
/// Plain `cargo test` (dev profile) measures with `opt-level = 1` for first-party crates per the
/// workspace root Cargo.toml, producing dev-profile speeds that are ~1.45x slower than release
/// (0.54x realtime vs. the actual 0.360x under --release). A dev-profile run can show false
/// failures near/over the 0.5x budget even though the actual (release-profile) pipeline
/// clears it comfortably.
/// `cfg.detector.warmup_hops` (see `track.rs`'s `DetectorConfig`) inhibits
/// ALL track creation for this many hops -- no decoder-pool work happens
/// in that window, only channelizer + noise-floor-estimator cost. At the
/// default 750 hops / `FO_HZ` (375 Hz hop rate), that's exactly 2.0s.
/// Dividing a ratio by the FULL scene duration therefore dilutes the
/// steady-state (all-300-tracks-active) cost by warmup/duration -- 2s of
/// 15s, ~13% -- understating the ratio relative to what ROADMAP.md's
/// "< X% of one core" criterion means (found via Codex review on this
/// PR). `decode_samples` has no public API to time only the post-warmup
/// portion of a single call (adding one would be a `manta-engine`
/// production API change, out of scope for this measurement-only ticket),
/// so this normalizes the denominator instead of pre-rolling: divide by
/// `(audio_duration - warmup)`, not raw `audio_duration`. That leaves the
/// warmup window's own small real cost folded into the numerator, i.e.
/// slightly *over*-corrects toward a stricter ratio -- the safer
/// direction to be wrong in for an accept gate. Derived from `cfg` at
/// runtime, not a hard-coded 750, so a future retune of
/// `DetectorConfig::default()` can't silently desync this from the
/// workload it's actually measuring (Codex review, this PR).
fn warmup_s(cfg: &PipelineConfig) -> f64 {
    cfg.detector.warmup_hops as f64 / manta_decode::FO_HZ
}

#[test]
#[ignore]
fn cpu_budget_mac_under_half_core() {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let audio_duration_s = iq.len() as f64 / fs;
    let warmup = warmup_s(&cfg);
    let steady_state_s = audio_duration_s - warmup;
    let cpu_before = cpu_seconds();
    let start = std::time::Instant::now();
    let report = manta_engine::decode_samples(&iq, fs, center_freq_hz, &cfg);
    let elapsed = start.elapsed().as_secs_f64();
    let cpu_elapsed = cpu_seconds() - cpu_before;
    let report = report.expect("decode_samples failed");

    // ROADMAP.md's 300-active-tracks scene only does its job if the
    // detector actually held ~300 tracks concurrently -- a detector/
    // config regression that churns through track IDs (opening and
    // closing far more than 300 over the run, none held for long) or
    // promotes fewer tracks at once would silently benchmark a cheaper
    // workload and could pass this gate for the wrong reason. See
    // `sustained_track_count`'s doc comment for why this is a proxy, not
    // an exact concurrent count.
    let total_samples = iq.len() as u64;
    let steady_state_start = (warmup * fs) as u64;
    let sustained = sustained_track_count(&report.events, steady_state_start, total_samples, fs);
    println!(
        "cpu_budget: {sustained} tracks sustained across most of the run (scene has 300 signals)"
    );
    // Exact 300, not a tolerance band: this scene is fully deterministic
    // (fixed seed, uniform 15 dB SNR, no fading/QSB, every signal
    // continuous for the whole 15s) and has produced exactly 300 on every
    // run measured so far -- a tolerance band here would let a workload up
    // to several percent cheaper than ROADMAP.md's 300-active-track
    // criterion silently pass, which matters given the Mac CPU-time margin
    // is only ~6-9% (see the decision doc).
    assert_eq!(
        sustained, 300,
        "only {sustained} of 300 scene signals produced a track sustained across most of the \
         run -- detector/config regression (fewer tracks held concurrently, or churn through \
         more than 300 short-lived track IDs) would silently cheapen this benchmark"
    );

    // Both ratios divide by steady_state_s (audio_duration_s minus the
    // detector warmup), not raw audio_duration_s -- see warmup_s's doc
    // comment above for why.
    let wall_ratio = elapsed / steady_state_s;
    // (user + sys) / audio_duration is the criterion ROADMAP.md actually
    // states ("< 1 full core" / "< 50% of one core") -- wall-clock alone
    // only equals it when the pipeline is single-core-bound. Measured
    // 2026-09-02: it isn't, quite -- decode_samples shows ~1.2x-1.25x
    // parallelism on this hardware (cpu_ratio > wall_ratio below), so both
    // ratios are asserted, not just wall-clock; a run that's fast in wall
    // time by spreading work across more cores than the budget allows
    // must still fail.
    let cpu_ratio = cpu_elapsed / steady_state_s;

    // Print BOTH ratios before either assert!. A Pi4 run failing the Mac
    // wall-clock bar (expected -- Pi4's own budget is a different, looser
    // number, < 1.0x) would otherwise panic on the wall_ratio assert below
    // before the CPU-time line ever printed, leaving the runbook's
    // instructed "read the core-seconds line" step with nothing to read
    // in exactly the slow-Pi scenario it exists for.
    println!(
        "cpu_budget: {elapsed:.2}s wall / {steady_state_s:.2}s steady-state audio ({audio_duration_s:.2}s scene minus {warmup:.1}s detector warmup) = {wall_ratio:.3}x realtime wall-clock (Mac budget: < 0.5x)"
    );
    // Unix-only -- see cpu_seconds' doc comment. cfg!(unix) rather than
    // #[cfg(unix)] on the block: both branches are ordinary std macro
    // calls (no platform-specific API), so gating at runtime keeps this
    // one block's control flow visible instead of duplicating the whole
    // tail of the function behind two #[cfg] copies.
    if cfg!(unix) {
        println!(
            "cpu_budget: {cpu_elapsed:.2}s (user+sys) CPU / {steady_state_s:.2}s steady-state audio = {cpu_ratio:.3}x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)"
        );
    } else {
        println!(
            "cpu_budget: (user+sys) CPU-time ratio not measured on this platform (Unix-only, see cpu_seconds' doc comment)"
        );
    }

    assert!(
        wall_ratio < 0.5,
        "192 kS/s / 300-track pipeline used {wall_ratio:.3}x realtime, Mac budget is < 0.5x (< 50% of one core)"
    );
    if cfg!(unix) {
        assert!(
            cpu_ratio < 0.5,
            "192 kS/s / 300-track pipeline used {cpu_ratio:.3}x (user+sys) core-seconds per \
             audio-second, Mac budget is < 0.5x (< 50% of one core) -- a run can pass on \
             wall-clock alone by spreading the same work across more cores than the budget allows"
        );
    }
}
