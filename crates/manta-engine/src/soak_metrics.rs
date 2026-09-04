//! Soak variant that also samples `TrackManager` stats (active track
//! count, per-`CloseReason` close counts) at a fixed interval, alongside
//! `soak.rs`'s existing panic/RSS-growth tracking -- MAN-19's 24h soak
//! needs "track count and evictions visible in metrics" throughout the
//! run, not just a single pass/fail verdict at the end.
//!
//! This duplicates `soak.rs`/`listen.rs`'s loop body rather than adding a
//! sampling hook to either: `TrackManager` isn't part of either function's
//! public surface, and `listen()` is a widely-used, already-reviewed
//! entry point (`manta listen`, `manta decode`, `soak()` itself) --
//! threading a new callback through it for one caller isn't worth the
//! risk. This file is net new; nothing in `soak.rs`/`listen.rs` changed.

use crate::track::{CloseCounts, TrackManager};
use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::Validator;
use num_complex::Complex32;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::track::CloseCounts as SoakCloseCounts;

const CHUNK_SAMPLES: usize = 2048;
const CALIBRATION_SECONDS: f64 = 2.0;
/// Matches `soak.rs::RSS_GROWTH_LIMIT_BYTES` -- same M1/M2/M3 soak-accept
/// threshold, reused here rather than re-litigated.
const RSS_GROWTH_LIMIT_BYTES: u64 = 200 * 1024 * 1024;
const WARMUP: Duration = Duration::from_secs(10);

/// One periodic observation, suitable for JSONL logging over a long run.
#[derive(Debug, Clone, Copy)]
pub struct SoakMetricsSample {
    pub elapsed_s: f64,
    pub rss_bytes: u64,
    pub events_emitted: usize,
    pub spots_emitted: usize,
    pub active_tracks: usize,
    pub close_counts: CloseCounts,
    /// MAN-3: total CANDIDATE -> ACTIVE promotions so far -- the only
    /// measurable handle on false-track pressure from a looser CANDIDATE
    /// confirmation rule (a track that promotes and dies before emitting
    /// anything is invisible to both `active_tracks` and `close_counts`'s
    /// `unconfirmed` bucket, which only counts CANDIDATE closes).
    pub promoted_count: u64,
}

#[derive(Debug)]
pub struct SoakMetricsReport {
    pub duration_actual: Duration,
    pub events_emitted: usize,
    pub spots_emitted: usize,
    pub rss_growth_bytes: u64,
    pub panicked: bool,
    pub peak_active_tracks: usize,
    pub final_close_counts: CloseCounts,
    /// Count of `DecoderEvent::TrackClosed` actually emitted -- distinct
    /// from `final_close_counts`'s total (which counts every close,
    /// including never-promoted CANDIDATEs that closed Unconfirmed
    /// without ever emitting anything real). This is specifically "how
    /// many tracks were promoted, emitted output, AND then closed" -- the
    /// only population that could have created `Validator`/
    /// `RepetitionGate` per-track_id state, and so the only meaningful
    /// evidence that the teardown path MAN-19 exists to soak-test was
    /// actually exercised (round 2 review).
    pub track_closed_events: usize,
    /// `Validator::gate_records_total` at the end of the run -- direct
    /// evidence `RepetitionGate::record` (and so `forget_track`'s half of
    /// the leak fix) was ever exercised. `track_closed_events` alone
    /// isn't enough: a track can promote, emit only TrackMeta/SpeedUpdate
    /// (no CharDecoded/word ever reaching the repetition gate), and still
    /// close -- satisfying `track_closed_events > 0` while
    /// `RepetitionGate::record` was never called (round 3 review).
    pub gate_records_total: u64,
    /// MAN-3: `TrackManager::promoted_count()` at the end of the run -- see
    /// `SoakMetricsSample::promoted_count`'s doc for why this exists
    /// alongside `final_close_counts`.
    pub promoted_count: u64,
}

// Windows has no libc::rusage/getrusage (POSIX-only) -- see this crate's
// Cargo.toml for why windows-sys is target-gated in as the equivalent.
// Duplicated from soak.rs's own peak_rss_bytes() (pre-existing
// duplication, not introduced by this fix) rather than extracted into a
// shared module -- out of scope for a build-bug fix.
#[cfg(unix)]
fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            raw // macOS reports ru_maxrss in bytes
        } else {
            raw * 1024 // Linux (and most others) report it in KB
        }
    }
}

#[cfg(windows)]
fn peak_rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let ok = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
        if ok != 0 {
            counters.PeakWorkingSetSize as u64
        } else {
            0
        }
    }
}

/// Run the streaming pipeline against `src` for `duration` wall-clock time
/// (or until EOF), sampling RSS + `TrackManager` stats every
/// `sample_interval` and feeding each sample to `on_sample`. Mirrors
/// `soak()`'s panic/RSS-growth tracking (`soak.rs`) and `listen()`'s loop
/// body (`listen.rs`) -- see module doc for why they're duplicated rather
/// than reused directly.
pub fn soak_with_metrics(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    duration: Duration,
    sample_interval: Duration,
    mut on_sample: impl FnMut(&SoakMetricsSample),
) -> Result<SoakMetricsReport> {
    // Validated before the watchdog thread is spawned, same reasoning as
    // soak.rs (MAN-29 review round 2): an invalid config must fail fast,
    // not after `duration` (potentially 24h) elapses.
    manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = stop.clone();
    // MAN-19 review round 1: distinct from `stop` (watchdog -> processing
    // loop). This one runs the other direction -- processing loop ->
    // watchdog -- so a source that ends (EOF/error/panic) well before
    // `duration` elapses doesn't leave the watchdog thread sleeping out
    // the rest of `duration` for nothing, and this function returns
    // promptly instead of silently blocking for up to a day.
    let done = Arc::new(AtomicBool::new(false));
    let done_watchdog = done.clone();
    let start = Instant::now();
    let baseline_rss = peak_rss_bytes();
    let mut worst_growth = 0u64;
    let mut event_count = 0usize;
    let mut spot_count = 0usize;
    let mut track_closed_count = 0usize;
    let mut peak_active_tracks = 0usize;
    let mut final_close_counts = CloseCounts::default();
    let mut gate_records_total = 0u64;
    let mut final_promoted_count = 0u64;
    let mut last_sample = Instant::now();

    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration {
            if done_watchdog.load(Ordering::Relaxed) {
                break;
            }
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(Duration::from_secs(1).min(remaining.max(Duration::from_millis(1))));
        }
        stop_watchdog.store(true, Ordering::Relaxed);
    });

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        // MAN-19 review round 1: unlike `listen()`'s `on_event` callback
        // (which the CLI's `--json` output needs calibrated), this
        // function never surfaces raw events to a caller -- only
        // `validator.ingest` sees them, and `Validator` already applies
        // `cfg.freq_correction_ppm` internally (`with_freq_correction_ppm`
        // below). Calibrating here too, as an earlier version of this
        // file did, double-applies the correction to every spot's
        // frequency (and to notch/dedupe bucketing) whenever
        // `freq_correction_ppm != 0.0` -- matching `lib.rs`'s
        // `calibrate_track_meta` doc, which explicitly warns against
        // feeding its output to `Validator::ingest`. Validated up front
        // only so a NaN/out-of-range ppm fails fast, same as soak.rs.
        manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
            .map_err(|e| anyhow::anyhow!(e))?;
        let fs = src.sample_rate();
        let center_freq_hz = src.center_freq_hz();

        let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
        let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
        let mut filled = 0;
        while filled < calib_n {
            let n = src.read(&mut calib[filled..])?;
            if n == 0 {
                anyhow::bail!("audio source ended during startup calibration");
            }
            filled += n;
        }
        let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
            .map_err(|e| anyhow::anyhow!(e))?;
        let hop = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            fs,
            center_freq_hz,
            cfg.detector,
            cfg.decode.clone(),
        );
        let mut validator = Validator::bundled(fs)
            .with_freq_correction_ppm(cfg.freq_correction_ppm)
            .map_err(|e| anyhow::anyhow!(e))?
            .with_blocklist(cfg.blocklist.clone())
            .with_notch(cfg.notch.clone());
        for call in &cfg.allowlist {
            validator.allowlist(call);
        }

        let pad_samples = ch.filter_len();
        let pad_hops = (pad_samples as u64).div_ceil(hop);
        let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
        for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
            event_count += 1;
            if matches!(ev, DecoderEvent::TrackClosed { .. }) {
                track_closed_count += 1;
            }
            spot_count += validator.ingest(&ev).len();
        }
        for ev in tm.process_hops(&ch.process(&calib), |m| m.saturating_sub(pad_hops) * hop) {
            event_count += 1;
            if matches!(ev, DecoderEvent::TrackClosed { .. }) {
                track_closed_count += 1;
            }
            spot_count += validator.ingest(&ev).len();
        }

        let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let n = src.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            for ev in tm.process_hops(&ch.process(&chunk[..n]), |m| {
                m.saturating_sub(pad_hops) * hop
            }) {
                event_count += 1;
                if matches!(ev, DecoderEvent::TrackClosed { .. }) {
                    track_closed_count += 1;
                }
                spot_count += validator.ingest(&ev).len();
            }
            // MAN-19 round 7: updated after every processed chunk, not
            // just at sample_interval ticks -- a track that opens AND
            // closes entirely between two ticks (plausible for a busy
            // pileup with a coarse interval) previously left no trace in
            // peak_active_tracks at all. Cheap: active_track_count() is
            // just self.tracks.len().
            let active = tm.active_track_count();
            peak_active_tracks = peak_active_tracks.max(active);

            if last_sample.elapsed() >= sample_interval {
                last_sample = Instant::now();
                final_close_counts = tm.close_counts();
                let rss = peak_rss_bytes();
                if start.elapsed() >= WARMUP {
                    worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
                }
                on_sample(&SoakMetricsSample {
                    elapsed_s: start.elapsed().as_secs_f64(),
                    rss_bytes: rss,
                    events_emitted: event_count,
                    spots_emitted: spot_count,
                    active_tracks: active,
                    close_counts: final_close_counts,
                    promoted_count: tm.promoted_count(),
                });
            }
        }
        // MAN-19 round 7: captured BEFORE finish(), which clears
        // self.tracks internally (round 4) -- calling
        // tm.active_track_count() after finish() always reads 0 against
        // an already-emptied map, silently contributing nothing to the
        // peak regardless of how many tracks were actually open right up
        // to end-of-stream.
        peak_active_tracks = peak_active_tracks.max(tm.active_track_count());
        for ev in tm.finish() {
            event_count += 1;
            if matches!(ev, DecoderEvent::TrackClosed { .. }) {
                track_closed_count += 1;
            }
            spot_count += validator.ingest(&ev).len();
        }
        final_close_counts = tm.close_counts();
        gate_records_total = validator.gate_records_total();
        final_promoted_count = tm.promoted_count();
        // MAN-19 review round 1: `worst_growth` was otherwise only ever
        // updated inside the periodic `sample_interval` branch above -- any
        // growth after the last tick (or the whole run, if
        // `sample_interval >= duration` or the source ends before one tick
        // fires) never got measured, so `rss_growth_bytes` could read well
        // below the true peak. One last reading here closes that gap.
        let rss = peak_rss_bytes();
        if start.elapsed() >= WARMUP {
            worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
        }
        Ok(())
    }));
    // MAN-19 review round 1: captured immediately once the processing
    // closure has actually returned (normally, by error, or by panic --
    // `catch_unwind` above already blocked until one of those happened),
    // not after `watchdog.join()` -- which can otherwise still be
    // sleeping out the rest of `duration` for a source that finished
    // early, inflating the reported duration to look like a full run that
    // did nothing for most of it.
    let processing_end = Instant::now();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    let panicked = match result {
        Ok(Ok(())) => false,
        Ok(Err(e)) => anyhow::bail!("soak_with_metrics loop returned an error (not a panic): {e}"),
        Err(_) => true,
    };

    Ok(SoakMetricsReport {
        duration_actual: processing_end.duration_since(start),
        events_emitted: event_count,
        spots_emitted: spot_count,
        rss_growth_bytes: worst_growth,
        panicked,
        peak_active_tracks,
        final_close_counts,
        track_closed_events: track_closed_count,
        gate_records_total,
        promoted_count: final_promoted_count,
    })
}

/// Same pass/fail bar as `soak_passed` (soak.rs): no panic, bounded RSS
/// growth. Track count/eviction visibility is a property of every
/// `SoakMetricsSample` emitted during the run, not a separate gate here.
pub fn soak_metrics_passed(report: &SoakMetricsReport) -> bool {
    !report.panicked && report.rss_growth_bytes < RSS_GROWTH_LIMIT_BYTES
}
