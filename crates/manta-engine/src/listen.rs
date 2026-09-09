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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
pub fn listen(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    on_event: impl FnMut(&DecoderEvent),
    on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
    listen_with_track_count(src, cfg, stop, on_event, on_spot, |_n| {})
}

/// `listen()` plus a live per-batch observer: `on_tracks` is called with
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
/// Kept as a separate entry point rather than a sixth parameter on
/// `listen()` so the existing callers and tests are untouched; `listen()`
/// is now a no-op-observer wrapper over this.
pub fn listen_with_track_count(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
    mut on_tracks: impl FnMut(usize),
) -> Result<()> {
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

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    on_tracks(tm.decoding_track_count());
    for ev in tm.process_hops(&ch.process(&calib), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    on_tracks(tm.decoding_track_count());

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
        on_tracks(tm.decoding_track_count());
    }
    for ev in tm.finish() {
        on_event(&crate::calibrate_freq_events(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    // `finish()` flushes and drops every decoder: nothing is being decoded
    // once the stream has ended, so the gauge must not be left holding the
    // last live value after a source disconnects or a replay hits EOF.
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
        listen_with_track_count(
            src,
            &PipelineConfig::default(),
            stop,
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
    /// `listen_with_track_count` before the end-of-stream `on_tracks(0)`,
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
        let err = listen_with_track_count(
            src,
            &PipelineConfig::default(),
            stop,
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
