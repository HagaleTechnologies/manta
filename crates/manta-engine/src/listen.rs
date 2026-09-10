//! Streaming pipeline: live/replayed audio -> PFB channelizer ->
//! `TrackManager` (SPEC §2), run continuously until Ctrl-C or EOF, emitting
//! the merged multi-track decode event stream as it's produced. No actor/
//! ring-thread split; see design doc §4.

use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::Validator;
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// Optional live handles into a running `listen()` loop, for a caller that
/// needs to observe engine-owned state the callbacks can't see.
///
/// MAN-45 (PR #63 round-9 finding): the daemon's `manta_active_tracks` gauge
/// reported a constant 0 because `listen()` owns its `TrackManager`
/// internally and exposed no live count -- `Metrics::set_active_tracks` had
/// no non-test caller. A shared atomic rather than another callback: the
/// consumer (`manta-cli`'s server runtime) polls on its own schedule and
/// must never be able to block the decode loop, which is exactly the shape
/// `IqSource::confirmed_live_handle` already uses for source liveness
/// (MAN-55).
#[derive(Clone, Default)]
pub struct ListenObservers {
    /// Updated after each processed chunk with `TrackManager::active_track_count()`.
    /// `None` (the default) skips the store entirely, so `listen()`'s
    /// existing callers -- including `soak()` and the CPU-budget bench --
    /// pay nothing.
    pub active_tracks: Option<Arc<AtomicU64>>,
}

/// Zeroes the active-track observer on EVERY exit path out of
/// `listen_with_observers`, not just the happy one.
///
/// MAN-45 (PR #63 round-19 finding, P2): `IqSource::read` failing after at
/// least one track had become active returned through `?` before
/// `tm.finish()` and the final `report_active_tracks`, leaving the observer
/// pinned at its last nonzero value while the `TrackManager` behind it was
/// already dropped -- a ghost count any long-lived consumer of
/// `listen_with_observers` would keep publishing after a device disconnect
/// or a file-read error. `manta-cli` happened to paper over this by zeroing
/// its own separate `Metrics` gauge; nothing made that true for other
/// callers. A `Drop` guard rather than cleanup appended to the end of the
/// function, because the error paths are precisely the ones that never
/// reached the end.
struct ActiveTracksGuard(Option<Arc<AtomicU64>>);

impl ActiveTracksGuard {
    /// Also zeroes on construction: a caller reusing one handle across runs
    /// (or one that seeded it with a nonzero value) must not see the
    /// previous run's tail count until the first chunk is processed.
    fn new(gauge: Option<Arc<AtomicU64>>) -> Self {
        let guard = Self(gauge);
        guard.clear();
        guard
    }

    fn clear(&self) {
        if let Some(gauge) = &self.0 {
            gauge.store(0, Ordering::Relaxed);
        }
    }
}

impl Drop for ActiveTracksGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
///
/// Unchanged entry point: `listen_with_observers` with no observers. Kept so
/// MAN-45's engine addition costs its four existing call sites nothing.
pub fn listen(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    on_event: impl FnMut(&DecoderEvent),
    on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
    listen_with_observers(
        src,
        cfg,
        stop,
        ListenObservers::default(),
        on_event,
        on_spot,
        |_n| {},
    )
}

/// Like `listen`, but additionally publishes engine-owned live state (the
/// active-track count) to the caller two ways: into `observers`, a shared
/// atomic a consumer on another thread can poll on its own schedule
/// (MAN-45), and to `on_tracks`, a synchronous per-batch callback (MAN-122).
/// Both carry the same number; they exist side by side because they answer
/// different questions -- see `ListenObservers`'s doc comment for why the
/// gauge is an atomic, and the `on_tracks` paragraphs below for why the
/// daemon additionally needs the per-batch *edge*.
///
/// `on_tracks` is called with
/// `TrackManager::decoding_track_count()` after every batch the pipeline
/// processes, including the final `finish()`, which reports 0.
///
/// MAN-122 review round 2: it fires on EVERY batch, not only when the
/// count changes. The call is the daemon's only per-batch signal that the
/// synchronous decode loop is still turning over -- a status line derived
/// from change-only notifications cannot tell "quiet band, count steady at
/// 2" from "`IqSource::read` wedged, count frozen at 2 since an hour ago",
/// which is exactly the question the line exists to answer. Suppressing
/// repeats is the caller's business now (`manta-cli` keeps the gauge store
/// unconditional -- one relaxed atomic per ~43 ms chunk is nothing against
/// the per-batch DSP work it follows).
///
/// MAN-122 review round 1: the daemon's `manta_active_tracks` gauge and its
/// status line's `tracks=` field must come from the manager's own lifecycle
/// state, not from the `DecoderEvent` stream `on_event` already sees. A
/// track that `TrackManager` has promoted but whose demodulator has not
/// latched emits no events at all -- `TrackDecoder` withholds `TrackMeta`
/// until `snr_2500_db()` is `Some`, and a silent ACTIVE track survives to
/// the ~30 s `gc_hops` GC -- so an event-derived count reports zero while
/// real decoders are running on weak or unmodulated signals. This is the
/// count that cannot lie about that.
///
/// Kept as a separate entry point rather than extra parameters on
/// `listen()` so the existing callers and tests are untouched; `listen()`
/// is now a no-op-observer wrapper over this.
pub fn listen_with_observers(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    observers: ListenObservers,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
    mut on_tracks: impl FnMut(usize),
) -> Result<()> {
    // Declared before the first `?` below so that EVERY early return from
    // here on clears the observer -- see `ActiveTracksGuard`'s doc comment.
    let _active_tracks_guard = ActiveTracksGuard::new(observers.active_tracks.clone());

    // Validated up front so a bad config value fails fast, before spending
    // CALIBRATION_SECONDS reading from a live device (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
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
    let mut tm = crate::track::TrackManager::new(
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

    // One relaxed store over a count that is a single filtered pass across
    // the (cap-bounded) track map, once per processed chunk and skipped
    // entirely when no observer is registered -- immaterial against the Pi4
    // CPU budget and against the channelizer + TrackManager work it follows.
    //
    // MAN-122: the published number is `decoding_track_count()`, NOT
    // `active_track_count()`. MAN-45 introduced this gauge against the
    // latter (every entry in `tracks`, unconfirmed CANDIDATEs included);
    // candidates are mostly noise-blip rise crossings that close within
    // `confirm_hops` without ever leasing a decoder, so counting them
    // inflates an operator-facing "is it decoding?" reading with signals
    // nothing is decoding. See `TrackManager::decoding_track_count`'s doc
    // comment for the full argument. `active_track_count` keeps its
    // existing meaning for `soak_metrics`' peak/eviction accounting.
    let report_active_tracks = |n_tracks: usize| {
        if let Some(gauge) = &observers.active_tracks {
            gauge.store(n_tracks as u64, Ordering::Relaxed);
        }
    };

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    let n_tracks = tm.decoding_track_count();
    report_active_tracks(n_tracks);
    on_tracks(n_tracks);
    for ev in tm.process_hops(&ch.process(&calib), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    let n_tracks = tm.decoding_track_count();
    report_active_tracks(n_tracks);
    on_tracks(n_tracks);

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // A read error must not escape via `?` before the gauge is
        // zeroed. The end-of-stream `on_tracks(0)` below is only reached
        // on a clean EOF or a `stop` request, so a mid-stream failure --
        // an SDR disconnecting, say -- would otherwise leave
        // `manta_active_tracks` (and the status line's `tracks=`) frozen
        // at its last nonzero value for the whole shutdown drain, up to
        // `SHUTDOWN_DRAIN_DEADLINE` (25 s), while the metrics listener is
        // still answering scrapes with decoders that no longer exist.
        // Publish 0 first, then propagate the original error unchanged.
        let n = match src.read(&mut chunk) {
            Ok(n) => n,
            Err(e) => {
                report_active_tracks(0);
                on_tracks(0);
                return Err(e);
            }
        };
        if n == 0 {
            break;
        }
        for ev in tm.process_hops(&ch.process(&chunk[..n]), |m| {
            m.saturating_sub(pad_hops) * hop
        }) {
            on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
            for spot in validator.ingest(&ev) {
                on_spot(&spot);
            }
        }
        let n_tracks = tm.decoding_track_count();
        report_active_tracks(n_tracks);
        on_tracks(n_tracks);
    }
    for ev in tm.finish() {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    // `finish()` flushes and drops every decoder and closes every remaining
    // track: nothing is being decoded once the stream has ended, so both
    // observers must settle back to 0 rather than be left holding the last
    // live value after a source disconnects or a replay hits EOF.
    report_active_tracks(0);
    on_tracks(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory IqSource for testing, reporting a fixed,
    /// caller-chosen `center_freq_hz` (unlike `AudioIqSource`, which always
    /// reports 0.0) -- this is what proves `listen()` actually reads
    /// `src.center_freq_hz()` instead of hardcoding 0.0.
    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FixedFreqSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-45 (PR #63 round-9 finding): `manta_active_tracks` reported a
    /// constant 0 on every production run because `listen()` exposed no
    /// live track count to its caller -- the manager's own count existed but
    /// was reachable only from inside the engine. This proves the observer
    /// handle tracks the real count during the run and settles at 0
    /// afterward (`TrackManager::finish()` closes every track). MAN-122: the
    /// number published here is `decoding_track_count()`, so an unconfirmed
    /// noise candidate can never lift it off 0.
    #[test]
    fn listen_with_observers_publishes_a_live_active_track_count() {
        use std::sync::atomic::AtomicU64;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let gauge = Arc::new(AtomicU64::new(0));
        let observed = gauge.clone();
        let mut peak = 0u64;
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
            },
            |_ev| peak = peak.max(observed.load(Ordering::Relaxed)),
            |_spot| {},
            |_n| {},
        )
        .unwrap();

        assert!(
            peak >= 1,
            "V1's single strong signal must show as an active track mid-run"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "finish() closes every track"
        );
    }

    /// MAN-45 (PR #63 round-19 finding, P2): an `IqSource::read` error after
    /// tracks had become active used to return through `?` with the observer
    /// still holding that nonzero count, so a caller polling the handle kept
    /// reporting ghost tracks for a `TrackManager` that no longer existed.
    #[test]
    fn the_active_track_observer_is_cleared_when_listen_exits_with_an_error() {
        use std::sync::atomic::AtomicU64;

        /// Fails its `read` as soon as the observer reports at least one
        /// active track -- exactly the "device disconnected mid-run" shape
        /// the finding describes.
        struct FailsOnceTracksAreActive {
            inner: FixedFreqSource,
            observed: Arc<AtomicU64>,
        }

        impl manta_input::IqSource for FailsOnceTracksAreActive {
            fn sample_rate(&self) -> f64 {
                self.inner.sample_rate()
            }
            fn center_freq_hz(&self) -> f64 {
                self.inner.center_freq_hz()
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                if self.observed.load(Ordering::Relaxed) > 0 {
                    anyhow::bail!("simulated device disconnect");
                }
                self.inner.read(buf)
            }
        }

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gauge = Arc::new(AtomicU64::new(0));
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsOnceTracksAreActive {
            inner: FixedFreqSource {
                samples: rendered.samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            observed: gauge.clone(),
        });

        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
            },
            |_ev| {},
            |_spot| {},
            |_n| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("simulated device disconnect"),
            "the read error must propagate, not be swallowed: {err}"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "the observer must not keep reporting tracks whose TrackManager is gone"
        );
    }

    /// The default path must stay exactly as cheap as before --
    /// `listen()`'s own signature and behavior are unchanged, and its four
    /// existing call sites (main.rs, soak.rs, two integration tests) do not
    /// move.
    #[test]
    fn plain_listen_still_runs_with_no_observers() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
    }

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        // A real V1-style golden signal (clean +20 dB tone), but fed through
        // a source that reports a nonzero center_freq_hz -- if listen() were
        // still hardcoding 0.0, every TrackMeta.freq_hz would come back as
        // just the +12.34 kHz baseband offset, not centered on 14 MHz.
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340 \
             (which is what a hardcoded center_freq_hz=0.0 would produce)",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-122 review round 1: the track-count observer reports
    /// `TrackManager`'s promoted-track count, rises above zero while a real
    /// signal is being decoded, and is driven back to zero by `finish()` so
    /// the daemon's gauge doesn't stay stuck at the last live value after
    /// EOF or an SDR disconnect. Review round 2: it also fires once per
    /// processed batch, repeats included -- that per-batch edge is what the
    /// daemon's status line uses to tell a wedged decode loop from a quiet
    /// band.
    #[test]
    fn listen_reports_the_managers_track_count_and_clears_it_at_end_of_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .unwrap();

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- the observer must see a promoted track at some point, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "finish() drops every decoder, so the final reported count must be 0, got {counts:?}"
        );
        // One call per batch: the padding batch, the calibration batch, one
        // per CHUNK_SAMPLES-sized read, and one final zero from finish().
        // Far more calls than there are distinct values -- the point being
        // that a steady count still produces a steady stream of calls.
        assert!(
            counts.len()
                > counts
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
            "the observer must fire per batch, repeats included, got {counts:?}"
        );
    }

    /// A source that replays `samples` and then FAILS instead of reporting
    /// EOF -- an SDR disconnecting mid-stream, not a file running out.
    struct FailsAtEndSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FailsAtEndSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if self.cursor >= self.samples.len() {
                anyhow::bail!("source disconnected");
            }
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-122 review round 3: a mid-stream `IqSource::read` failure exits
    /// `listen_with_observers` before the end-of-stream `on_tracks(0)`,
    /// so without an explicit zero on the error path the daemon's
    /// `manta_active_tracks` gauge (and the status line's `tracks=`) would
    /// stay frozen at its last nonzero value for the whole shutdown drain
    /// while the metrics listener still answers scrapes.
    #[test]
    fn listen_zeroes_the_track_count_when_a_read_fails_mid_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsAtEndSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .expect_err("the source fails instead of reaching EOF, so listen must propagate the error");
        assert!(
            err.to_string().contains("source disconnected"),
            "the original read error must be propagated unchanged, got {err}"
        );

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- a track must have been promoted before the failure, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "a failed read must publish 0 before propagating, or the gauge stays frozen through shutdown drain, got {counts:?}"
        );
    }

    /// MAN-29: `PipelineConfig::freq_correction_ppm` reaches the emitted
    /// spot's `freq_hz`, corrected by the configured ppm -- end-to-end
    /// through `listen()`, not just the `manta-spot::Validator` unit.
    #[test]
    fn listen_applies_freq_correction_ppm_to_emitted_spot_freq_hz() {
        const PPM: f64 = 10.0; // ~140 Hz at 14 MHz.
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(src, &cfg, stop, |_ev| {}, |spot| spots.push(spot.clone())).unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
        for spot in &spots {
            let uncorrected = spot.freq_hz / factor;
            assert!(
                (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
                "spot.freq_hz {} divided back by the calibration factor should land near the \
                 raw decoded frequency {}, proving the correction was applied once, multiplicatively",
                spot.freq_hz,
                spec.center_freq_hz + 12_340.0
            );
        }
    }

    /// MAN-29 review round 3: the `TrackMeta` events `listen()` passes to
    /// `on_event` (consumed directly by `listen --json`) must be
    /// calibrated too, not just the emitted spots.
    #[test]
    fn listen_calibrates_track_meta_events_passed_to_on_event() {
        const PPM: f64 = 10.0;
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &cfg,
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        let uncorrected = freq_hz / factor;
        assert!(
            (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "on_event's TrackMeta.freq_hz {freq_hz} divided back by the calibration factor \
             should land near the raw decoded frequency {}",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-29 review: an invalid `freq_correction_ppm` must fail `listen()`
    /// up front rather than silently poisoning spot output.
    #[test]
    fn listen_rejects_an_invalid_freq_correction_ppm() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        assert!(listen(src, &cfg, stop, |_ev| {}, |_spot| {}).is_err());
    }

    /// MAN-31: `listen()` is the other production call site that must
    /// apply an operator-supplied suppression list.
    #[test]
    fn listen_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let mut spots = Vec::new();
        listen(
            src,
            &cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(
            spots.is_empty(),
            "blocklisted callsign must never be spotted, got {spots:?}"
        );
    }
}
