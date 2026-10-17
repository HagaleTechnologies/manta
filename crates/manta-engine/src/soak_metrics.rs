//! Soak wrapper that samples RSS and track lifecycle counters at a fixed
//! interval, alongside panic and RSS-growth tracking for MAN-19's 24h soak.
//!
//! Since MAN-236 this calls `listen_with_observers`, the loop `manta run`
//! executes. `TrackLifecycleObserver` and the static-crash observer provide
//! the counters; no DSP or validator loop is maintained here.

use crate::soak::{peak_rss_bytes, RSS_GROWTH_LIMIT_BYTES, WARMUP};
use crate::track::CloseCounts;
use crate::{PipelineConfig, TrackLifecycleObserver};
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::track::CloseCounts as SoakCloseCounts;

/// One periodic observation, suitable for JSONL logging over a long run.
#[derive(Debug, Clone, Copy)]
pub struct SoakMetricsSample {
    pub elapsed_s: f64,
    pub rss_bytes: u64,
    pub events_emitted: usize,
    pub spots_emitted: usize,
    pub active_tracks: usize,
    pub close_counts: CloseCounts,
    /// MAN-312: `TrackManager::static_crash_count` at this sample.
    pub static_crashes: u64,
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
    /// MAN-312: static crashes detected over the whole run.
    pub final_static_crashes: u64,
}

/// Run the streaming pipeline against `src` for `duration` wall-clock time
/// (or until EOF), sampling RSS + `TrackManager` stats every
/// `sample_interval` and feeding each sample to `on_sample`. Uses the shipped
/// streaming loop and shares `soak()`'s panic/RSS-growth acceptance bar.
pub fn soak_with_metrics(
    src: Box<dyn IqSource>,
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
    let lifecycle = Arc::new(TrackLifecycleObserver::default());
    let static_crashes = Arc::new(AtomicU64::new(0));
    let event_count = Cell::new(0usize);
    let spot_count = Cell::new(0usize);
    let mut track_closed_count = 0usize;
    let mut seen_chunks = 0u64;
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
        // MAN-19 review round 1: only the event type is counted here.
        // `listen_with_observers` sends its calibrated copy to `on_event`
        // and the original to the validator, which applies ppm internally.
        // Never feed the calibrated copy back into validation: that would
        // double-correct frequencies and notch/dedupe bucketing.
        crate::listen_with_observers(
            src,
            cfg,
            stop.clone(),
            crate::ListenObservers {
                track_lifecycle: Some(lifecycle.clone()),
                static_crashes: Some(static_crashes.clone()),
                ..Default::default()
            },
            |ev| {
                event_count.set(event_count.get() + 1);
                if matches!(ev, DecoderEvent::TrackClosed { .. }) {
                    track_closed_count += 1;
                }
            },
            |_spot| spot_count.set(spot_count.get() + 1),
            |_decoding| {
                let snap = lifecycle.snapshot();
                // `on_tracks` also fires for padding, calibration and finish.
                // MAN-19 samples only after steady-state chunks.
                if snap.chunks == seen_chunks {
                    return;
                }
                seen_chunks = snap.chunks;
                if last_sample.elapsed() >= sample_interval {
                    last_sample = Instant::now();
                    let rss = peak_rss_bytes();
                    if start.elapsed() >= WARMUP {
                        worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
                    }
                    on_sample(&SoakMetricsSample {
                        elapsed_s: start.elapsed().as_secs_f64(),
                        rss_bytes: rss,
                        events_emitted: event_count.get(),
                        spots_emitted: spot_count.get(),
                        active_tracks: snap.active_tracks as usize,
                        close_counts: snap.close_counts,
                        static_crashes: static_crashes.load(Ordering::Relaxed),
                    });
                }
            },
        )?;
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

    let snap = lifecycle.snapshot();
    Ok(SoakMetricsReport {
        duration_actual: processing_end.duration_since(start),
        events_emitted: event_count.get(),
        spots_emitted: spot_count.get(),
        rss_growth_bytes: worst_growth,
        panicked,
        peak_active_tracks: snap.peak_active_tracks as usize,
        final_close_counts: snap.close_counts,
        track_closed_events: track_closed_count,
        gate_records_total: snap.gate_records_total,
        final_static_crashes: static_crashes.load(Ordering::Relaxed),
    })
}

/// Same pass/fail bar as `soak_passed` (soak.rs): no panic, bounded RSS
/// growth. Track count/eviction visibility is a property of every
/// `SoakMetricsSample` emitted during the run, not a separate gate here.
pub fn soak_metrics_passed(report: &SoakMetricsReport) -> bool {
    !report.panicked && report.rss_growth_bytes < RSS_GROWTH_LIMIT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    struct VecSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        /// MAN-196: `None` mimics every non-decimated source (the trait
        /// default); `Some` mimics a `DecimatingSource`.
        trusted_band_hz: Option<(f64, f64)>,
    }

    impl IqSource for VecSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn trusted_band_hz(&self) -> Option<(f64, f64)> {
            self.trusted_band_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    fn source(samples: Vec<Complex32>) -> Box<dyn IqSource> {
        Box::new(VecSource {
            samples,
            cursor: 0,
            fs: 48_000.0,
            trusted_band_hz: None,
        })
    }

    fn v1_samples() -> Vec<Complex32> {
        let mut spec = manta_testkit::vectors::v1();
        spec.fs = 48_000.0;
        spec.duration_s = 20.0;
        spec.center_freq_hz = 0.0;
        manta_testkit::vectors::render(&spec).unwrap().samples
    }

    fn close_fields(c: CloseCounts) -> [u64; 8] {
        [
            c.unconfirmed,
            c.hang_expired,
            c.silent,
            c.merged,
            c.evicted,
            c.rtty,
            c.not_cw,
            c.unkeyed,
        ]
    }

    #[test]
    fn soak_with_metrics_samples_once_per_steady_state_chunk() {
        let mut samples_seen = 0;
        soak_with_metrics(
            source(vec![Complex32::new(0.0, 0.0); 96_000 + 3 * 2048 + 100]),
            &PipelineConfig::default(),
            Duration::from_secs(600),
            Duration::ZERO,
            |_| samples_seen += 1,
        )
        .unwrap();
        assert_eq!(samples_seen, 4);
    }

    #[test]
    fn soak_with_metrics_samples_never_exceed_the_report() {
        let mut samples = Vec::new();
        let report = soak_with_metrics(
            source(v1_samples()),
            &PipelineConfig::default(),
            Duration::from_secs(600),
            Duration::ZERO,
            |s| samples.push(*s),
        )
        .unwrap();
        assert!(!samples.is_empty());
        assert!(report.events_emitted > 0 && report.peak_active_tracks > 0);
        for pair in samples.windows(2) {
            assert!(pair[0].events_emitted <= pair[1].events_emitted);
            assert!(pair[0].spots_emitted <= pair[1].spots_emitted);
            assert!(pair[0].static_crashes <= pair[1].static_crashes);
            for (before, after) in close_fields(pair[0].close_counts)
                .into_iter()
                .zip(close_fields(pair[1].close_counts))
            {
                assert!(before <= after);
            }
        }
        for sample in &samples {
            assert!(sample.active_tracks <= report.peak_active_tracks);
            assert!(sample.events_emitted <= report.events_emitted);
            assert!(sample.spots_emitted <= report.spots_emitted);
            assert!(sample.static_crashes <= report.final_static_crashes);
        }
        for (last, total) in close_fields(samples.last().unwrap().close_counts)
            .into_iter()
            .zip(close_fields(report.final_close_counts))
        {
            assert!(last <= total);
        }
    }

    #[test]
    fn soak_with_metrics_counts_the_stream_listen_produces() {
        let samples = v1_samples();
        let mut events = Vec::new();
        let mut spots = Vec::new();
        crate::listen(
            source(samples.clone()),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |ev| events.push(ev.clone()),
            |spot| spots.push(spot.clone()),
        )
        .unwrap();
        let report = soak_with_metrics(
            source(samples),
            &PipelineConfig::default(),
            Duration::from_secs(600),
            Duration::ZERO,
            |_| {},
        )
        .unwrap();
        assert_eq!(report.events_emitted, events.len());
        assert_eq!(report.spots_emitted, spots.len());
        assert_eq!(
            report.track_closed_events,
            events
                .iter()
                .filter(|ev| matches!(ev, DecoderEvent::TrackClosed { .. }))
                .count()
        );
    }

    /// MAN-312: the soak report carries the run's static-crash count.
    #[test]
    fn soak_report_counts_static_crashes() {
        let fs = 96_000.0;
        let mut samples = manta_testkit::crash::vc1_scene(fs);
        manta_testkit::crash::add_crash(
            &mut samples,
            fs,
            &manta_testkit::crash::vc1_crash(fs, 5.0, 40.0),
        );
        let src = Box::new(VecSource {
            samples,
            cursor: 0,
            fs,
            trusted_band_hz: None,
        });
        let report = soak_with_metrics(
            src,
            &PipelineConfig::default(),
            Duration::from_secs(600),
            Duration::from_secs(600),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.final_static_crashes, 1);
    }

    /// MAN-196: `soak_with_metrics` applies the source's trusted band to
    /// its track manager exactly as `listen` does, so a decimated source
    /// spawns no track in the decimator's transition band on this path
    /// either. Same scene as listen's
    /// `trusted_band_vetoes_spawns_beyond_the_decimator_band`.
    #[test]
    fn soak_with_metrics_applies_the_trusted_band() {
        let mut spec = manta_testkit::vectors::v1();
        spec.fs = 48_000.0;
        spec.duration_s = 20.0;
        spec.signals[0].offset_hz = 23_500.0;
        spec.signals[0].snr_2500_db = 10.0;
        spec.center_freq_hz = 0.0;
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let trusted = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * spec.fs;

        let peak_tracks = |trusted_band_hz: Option<(f64, f64)>| {
            let src = Box::new(VecSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                trusted_band_hz,
            });
            soak_with_metrics(
                src,
                &PipelineConfig::default(),
                Duration::from_secs(600),
                Duration::from_secs(600),
                |_| {},
            )
            .unwrap()
            .peak_active_tracks
        };

        assert!(
            peak_tracks(None) >= 1,
            "control without a trusted band must detect the signal"
        );
        assert_eq!(
            peak_tracks(Some((-trusted, trusted))),
            0,
            "no track may spawn beyond the trusted band in soak_with_metrics"
        );
    }
}
