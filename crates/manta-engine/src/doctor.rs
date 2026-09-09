//! `manta doctor`: a bounded-duration health check that answers "is this
//! source hearing anything real?" independent of whether a spot ever
//! validates -- the question a bare `listen()` run can't answer on its own
//! (a silent antenna, a quiet band, and a genuine decoder-tuning gap all
//! look identical from the outside: zero spots). Reuses `listen()`'s real
//! channelizer/track-manager pipeline verbatim; this module only tabulates
//! its existing `DecoderEvent`/`Spot` stream over a fixed window and adds a
//! verdict, the same tabulation this project did by hand (`jq` over
//! `listen --json` output) against a real SDRplay RSP1B -- see
//! docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md.

use crate::{listen, PipelineConfig};
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A run whose STRONGEST `TrackMeta.snr_2500_db` (the max, not the median --
/// see `verdict()`) sits at or below this reads as noise-floor chatter
/// rather than a real signal. NOT a SPEC-defined value -- a doctor-local
/// heuristic, calibrated against the real-hardware runs in
/// docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md, where every track
/// opened against band noise alone (no confirmed spot, an antenna that
/// really was hearing real HF energy elsewhere in the same session) topped
/// out at -0.9 dB. 0.0 dB is a deliberately conservative line above that
/// observed ceiling, not a tight fit to it -- retune if it misclassifies a
/// real run.
pub const NOISE_FLOOR_SNR_DB: f32 = 0.0;

/// `doctor()`'s classification of a run, from cheapest/most-damning signal
/// to most reassuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Verdict {
    /// No decoder evidence at all the whole run -- no `TrackMeta`, no
    /// `CharDecoded`, no confirmed spot. The detector never even opened a
    /// track. Check the antenna, the gain, and that the tuned passband
    /// actually overlaps where you expect signal.
    NoSignal,
    /// Decoder evidence exists (a closed track, or decoded characters),
    /// but not one `TrackMeta` landed before the last such track closed --
    /// no SNR was ever measured. Distinct from `NoisyNoDecode`: this is
    /// "unmeasured," not "measured and low."
    ActivityNoSnr,
    /// Tracks opened, but every one reads at or below `NOISE_FLOOR_SNR_DB`
    /// and none produced a confirmed spot -- looks like noise-floor
    /// chatter, not real traffic, at least on this passband right now.
    NoisyNoDecode,
    /// Tracks opened with real signal-level SNR, but nothing validated
    /// into a confirmed spot -- a genuine signal is present but isn't
    /// decoding as CW (wrong mode, WPM out of range, grammar/repetition
    /// gate rejecting every candidate).
    WeakNoDecode,
    /// At least one confirmed spot. The whole path -- source, channelizer,
    /// track manager, decoder, validator -- is working end to end.
    Decoding,
}

impl Verdict {
    pub fn summary(&self) -> &'static str {
        match self {
            Verdict::NoSignal => {
                "NO_SIGNAL -- no decoder evidence at all (no track, no decoded character, no \
                 spot). Check antenna, gain, and that the tuned passband overlaps where you \
                 expect signal."
            }
            Verdict::ActivityNoSnr => {
                "ACTIVITY_NO_SNR -- decoder activity happened (a track closed and/or characters \
                 decoded) but no SNR was ever measured before it closed, and nothing confirmed \
                 a spot. Can't yet say noise vs. real signal -- try a longer --duration."
            }
            Verdict::NoisyNoDecode => {
                "NOISY_NO_DECODE -- tracks opened but every one reads at or below the noise \
                 floor and nothing validated into a confirmed spot. Looks like noise, not real \
                 CW traffic, right now (characters may still have been decoded off the noise \
                 floor -- see chars_decoded)."
            }
            Verdict::WeakNoDecode => {
                "WEAK_NO_DECODE -- real signal-level SNR seen, but nothing validated into a \
                 spot. Signal is present; something else (mode, WPM, grammar gate) is rejecting \
                 it."
            }
            Verdict::Decoding => "DECODING -- at least one confirmed spot. End to end, working.",
        }
    }
}

/// Tabulated result of one `doctor()` run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    /// `IqSource::sample_rate()`, as actually negotiated (SoapySDR and
    /// similar sources round to the nearest achievable rate; this is truth,
    /// not the ask).
    pub sample_rate_hz: f64,
    /// `IqSource::center_freq_hz()`, as actually negotiated.
    pub center_freq_hz: f64,
    /// How long the run actually observed the source for.
    pub duration: Duration,
    /// Number of `TrackPromoted` events seen -- the detector's own ground
    /// truth for "found a candidate signal," independent of whether the
    /// decoder ever got far enough to produce a `TrackMeta`/`CharDecoded`/
    /// `TrackClosed` afterward. This, not any decode-timing proxy, is what
    /// `verdict()`'s `NoSignal` check keys off (see
    /// docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md).
    pub tracks_promoted: usize,
    /// Number of `TrackMeta` events seen (one per track, per hop it's
    /// updated -- a proxy for how much decoder activity there was, not a
    /// distinct-track count).
    pub track_meta_count: usize,
    /// Number of `TrackClosed` events seen. `TrackManager` only emits
    /// `TrackClosed` for a track that produced at least one other event
    /// before closing (`has_emitted`, MAN-19, `track.rs`) -- an unpromoted
    /// CANDIDATE, or a promoted track merged/evicted before its first
    /// `process_hops` pass, closes with no event at all. So this counts
    /// *eventful* tracks that closed, not every detector candidate opened
    /// during the run -- use `tracks_promoted` for that.
    pub tracks_closed: usize,
    pub snr_db_min: Option<f32>,
    pub snr_db_max: Option<f32>,
    /// Median of every `TrackMeta.snr_2500_db` seen, in dB. `None` if no
    /// `TrackMeta` event occurred.
    pub snr_db_median: Option<f32>,
    /// Total `CharDecoded` events across every track.
    pub chars_decoded: usize,
    /// Distinct decoded characters seen (via `Glyph::text_char`). Real CW
    /// text uses a broad alphabet; a noise-floor track tends to collapse
    /// onto one or two characters (`E`/`T`, the shortest morse elements)
    /// repeated at length.
    pub distinct_chars: usize,
    /// Confirmed, fully-validated spots -- the strictest signal.
    pub spots_confirmed: usize,
}

impl DoctorReport {
    pub fn verdict(&self) -> Verdict {
        if self.spots_confirmed > 0 {
            return Verdict::Decoding;
        }
        // `tracks_promoted` is the detector's own ground truth, not a
        // decode-timing proxy (TrackMeta/CharDecoded/TrackClosed all
        // depend on the decoder getting further than mere promotion --
        // e.g. TrackMeta needs ~1s of decoder init on top of the ~2.05s
        // worst-case warmup+confirm-hops promotion latency, which a short
        // --duration run can end before reaching at all). Promotion alone
        // is what "the detector found a candidate" means.
        if self.tracks_promoted == 0 {
            return Verdict::NoSignal;
        }
        // Based on the STRONGEST track seen (max), not the median: a
        // wideband run can have several noise-floor tracks alongside one
        // real above-threshold carrier, which would otherwise average out
        // to a median at or below the noise floor and hide the one real
        // signal that was actually observed. `None` here means decoder
        // evidence exists (checked above) but not one TrackMeta ever
        // landed -- genuinely unmeasured, not "measured and low" --
        // reported distinctly rather than folded into NoisyNoDecode.
        match self.snr_db_max {
            Some(max) if max > NOISE_FLOOR_SNR_DB => Verdict::WeakNoDecode,
            Some(_) => Verdict::NoisyNoDecode,
            None => Verdict::ActivityNoSnr,
        }
    }
}

fn median(mut values: Vec<f32>) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

/// Minimum accepted `--duration`. `listen()`'s fixed ~2s startup
/// calibration read loop (`CALIBRATION_SECONDS`, private to `listen.rs`)
/// isn't stop-aware -- it reads and analyzes that window regardless of
/// `stop`, so a shorter requested duration would silently overrun it while
/// still reporting the shorter number back. Comfortably above that 2s
/// floor, not a tight fit to it. Kept local to doctor rather than plumbed
/// into `listen()` itself, since ordinary `manta listen` has no duration
/// bound to enforce in the first place.
pub const MIN_DURATION: Duration = Duration::from_secs(3);

/// Maximum accepted `--duration`. `doctor()` accumulates every `TrackMeta`
/// SNR sample in memory for the whole run to compute `snr_db_median` at the
/// end -- on a noisy full passband with sustained track churn, an
/// unbounded duration means unbounded memory (tens of millions of floats
/// over a 24h+ run). `doctor` is a bounded-duration health check, not a
/// long-running monitor (that's `manta listen --server-config`'s
/// Prometheus metrics), so a generous but real ceiling is the right fix,
/// not a fancier streaming-quantile estimator for a tool that was never
/// meant to run for hours in the first place.
pub const MAX_DURATION: Duration = Duration::from_secs(3600);

/// Run `listen()` against `src` for `duration`, tabulating its event/spot
/// stream into a `DoctorReport` instead of printing or serving it. Reuses
/// the real pipeline verbatim (same channelizer, same `TrackManager`, same
/// `Validator`) -- doctor never reimplements or approximates the DSP, it
/// only observes the same event stream `listen --json` already exposes.
pub fn doctor(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    duration: Duration,
) -> Result<DoctorReport> {
    manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;
    if duration < MIN_DURATION {
        anyhow::bail!(
            "--duration must be at least {}s -- listen()'s fixed startup calibration window \
             isn't stop-aware, so a shorter duration would silently overrun it",
            MIN_DURATION.as_secs()
        );
    }
    if duration > MAX_DURATION {
        anyhow::bail!(
            "--duration must be at most {}s -- doctor() accumulates every TrackMeta SNR sample \
             in memory for the run's duration, so an unbounded --duration means unbounded \
             memory; for a long-running check, use `manta listen --server-config` instead",
            MAX_DURATION.as_secs()
        );
    }

    let sample_rate_hz = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = stop.clone();
    // Set right after `listen()` returns (below), whether that's because
    // `duration` elapsed or because the source hit EOF/an error early --
    // lets the watchdog thread wake and exit immediately instead of
    // sleeping out the rest of `duration` before `watchdog.join()` can
    // return, which would otherwise make a short-input or early-error run
    // block for the full requested duration regardless.
    let listen_done = Arc::new(AtomicBool::new(false));
    let watchdog_done = listen_done.clone();
    let start = Instant::now();

    const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration && !watchdog_done.load(Ordering::Relaxed) {
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(SAMPLE_INTERVAL.min(remaining.max(Duration::from_millis(1))));
        }
        stop_watchdog.store(true, Ordering::Relaxed);
    });

    let mut tracks_promoted = 0usize;
    let mut track_meta_count = 0usize;
    let mut tracks_closed = 0usize;
    let mut snrs: Vec<f32> = Vec::new();
    let mut chars_decoded = 0usize;
    let mut distinct_chars: HashSet<char> = HashSet::new();
    let mut spots_confirmed = 0usize;

    let result = listen(
        src,
        cfg,
        stop.clone(),
        |ev| match ev {
            DecoderEvent::TrackPromoted { .. } => tracks_promoted += 1,
            DecoderEvent::TrackMeta { snr_2500_db, .. } => {
                track_meta_count += 1;
                snrs.push(*snr_2500_db);
            }
            DecoderEvent::TrackClosed { .. } => tracks_closed += 1,
            DecoderEvent::CharDecoded { glyph, .. } => {
                chars_decoded += 1;
                if let Some(c) = glyph.text_char() {
                    distinct_chars.insert(c);
                }
            }
            _ => {}
        },
        |_spot| spots_confirmed += 1,
    );
    let observed_duration = start.elapsed();
    listen_done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    result?;

    let snr_db_min = snrs.iter().copied().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.min(v)))
    });
    let snr_db_max = snrs.iter().copied().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.max(v)))
    });
    let snr_db_median = median(snrs);

    Ok(DoctorReport {
        sample_rate_hz,
        center_freq_hz,
        duration: observed_duration,
        tracks_promoted,
        track_meta_count,
        tracks_closed,
        snr_db_min,
        snr_db_max,
        snr_db_median,
        chars_decoded,
        distinct_chars: distinct_chars.len(),
        spots_confirmed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_input::AudioIqSource;
    use num_complex::Complex32;

    /// Minimal in-memory `IqSource` reporting `vectors::v1()`'s own
    /// `fs`/`center_freq_hz` directly -- `AudioIqSource` requires exactly
    /// 48000 Hz input and v1's fixture isn't necessarily that rate, so
    /// this bypasses it entirely (mirrors `listen.rs`'s own
    /// `FixedFreqSource` test helper).
    struct RawIqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl IqSource for RawIqSource {
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

    fn v1_source() -> Box<dyn IqSource> {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        Box::new(RawIqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        })
    }

    #[test]
    fn doctor_reports_decoding_on_a_clean_golden_signal() {
        let report = doctor(
            v1_source(),
            &PipelineConfig::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(report.verdict(), Verdict::Decoding);
        assert!(report.spots_confirmed > 0);
        assert!(report.track_meta_count > 0);
        assert!(report.tracks_promoted > 0);
    }

    #[test]
    fn doctor_reports_no_signal_on_digital_silence() {
        let fs = manta_input::TARGET_RATE_HZ;
        let silence = vec![0.0f32; fs as usize * 4];
        let src: Box<dyn IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(silence, fs)))
                .unwrap(),
        );
        let report = doctor(src, &PipelineConfig::default(), Duration::from_secs(3)).unwrap();
        assert_eq!(report.verdict(), Verdict::NoSignal);
        assert_eq!(report.tracks_promoted, 0);
        assert_eq!(report.track_meta_count, 0);
        assert_eq!(report.spots_confirmed, 0);
    }

    #[test]
    fn doctor_rejects_an_invalid_freq_correction_ppm_before_the_watchdog_duration() {
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let start = Instant::now();
        let result = doctor(v1_source(), &cfg, Duration::from_secs(20));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "doctor() with an invalid freq_correction_ppm took {:?} -- it must fail before \
             joining the watchdog, not wait out the requested duration",
            start.elapsed()
        );
    }

    #[test]
    fn verdict_classifies_noisy_no_decode_below_the_noise_floor_threshold() {
        let report = DoctorReport {
            sample_rate_hz: 192_000.0,
            center_freq_hz: 14_025_000.0,
            duration: Duration::from_secs(10),
            tracks_promoted: 3,
            track_meta_count: 5,
            tracks_closed: 3,
            snr_db_min: Some(-8.0),
            snr_db_max: Some(-1.0),
            snr_db_median: Some(-6.0),
            chars_decoded: 50,
            distinct_chars: 2,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::NoisyNoDecode);
    }

    #[test]
    fn verdict_classifies_weak_no_decode_above_the_noise_floor_threshold() {
        let report = DoctorReport {
            sample_rate_hz: 192_000.0,
            center_freq_hz: 14_025_000.0,
            duration: Duration::from_secs(10),
            tracks_promoted: 1,
            track_meta_count: 5,
            tracks_closed: 1,
            snr_db_min: Some(5.0),
            snr_db_max: Some(15.0),
            snr_db_median: Some(10.0),
            chars_decoded: 200,
            distinct_chars: 20,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::WeakNoDecode);
    }

    /// Regression: a wideband run with several noise-floor tracks (pulling
    /// the median down) alongside one real above-threshold carrier must
    /// still classify as WeakNoDecode, not NoisyNoDecode -- verdict() must
    /// key off the strongest track observed (max), not the median.
    #[test]
    fn verdict_uses_the_strongest_track_not_the_median() {
        let report = DoctorReport {
            sample_rate_hz: 192_000.0,
            center_freq_hz: 14_025_000.0,
            duration: Duration::from_secs(10),
            tracks_promoted: 16,
            track_meta_count: 20,
            tracks_closed: 15,
            snr_db_min: Some(-8.0),
            snr_db_max: Some(12.0),
            snr_db_median: Some(-6.0),
            chars_decoded: 300,
            distinct_chars: 3,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::WeakNoDecode);
    }

    /// Regression: TrackMeta is only emitted periodically, so a short
    /// track can decode characters (or even confirm an allowlisted spot)
    /// and close before its first TrackMeta update ever lands. That must
    /// not read as NoSignal -- direct decode evidence exists.
    #[test]
    fn verdict_does_not_report_no_signal_when_chars_decoded_without_track_meta() {
        let report = DoctorReport {
            sample_rate_hz: 48_000.0,
            center_freq_hz: 0.0,
            duration: Duration::from_secs(3),
            tracks_promoted: 1,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 4,
            distinct_chars: 3,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::ActivityNoSnr);
    }

    /// Regression: `tracks_closed` (see its doc comment) only ever counts
    /// an eventful closure -- a track that closes having emitted only
    /// WordBoundary/SpeedUpdate (no TrackMeta, no CharDecoded) still proves
    /// real decoder activity and must not read as NoSignal.
    #[test]
    fn verdict_does_not_report_no_signal_when_a_track_closed_eventfully() {
        let report = DoctorReport {
            sample_rate_hz: 48_000.0,
            center_freq_hz: 0.0,
            duration: Duration::from_secs(3),
            tracks_promoted: 1,
            track_meta_count: 0,
            tracks_closed: 1,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::ActivityNoSnr);
    }

    /// Regression (round-4 review finding): a real signal rising just
    /// after the ~2s detector warmup can reach `finish()` having only
    /// promoted -- no TrackMeta (needs ~1s more decoder init), no
    /// CharDecoded, no eventful TrackClosed (has_emitted stays false for a
    /// promoted-but-otherwise-silent track). `TrackPromoted` is the one
    /// signal that still fires in this exact case; must not read as
    /// NoSignal.
    #[test]
    fn verdict_does_not_report_no_signal_when_only_promoted() {
        let report = DoctorReport {
            sample_rate_hz: 48_000.0,
            center_freq_hz: 0.0,
            duration: Duration::from_secs(3),
            tracks_promoted: 1,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
        };
        assert_eq!(report.verdict(), Verdict::ActivityNoSnr);
    }

    #[test]
    fn doctor_rejects_a_duration_longer_than_the_max() {
        let start = Instant::now();
        let result = doctor(
            v1_source(),
            &PipelineConfig::default(),
            MAX_DURATION + Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "doctor() with a too-long --duration took {:?} -- it must reject before running \
             the pipeline",
            start.elapsed()
        );
    }

    #[test]
    fn doctor_rejects_a_duration_shorter_than_startup_calibration() {
        let start = Instant::now();
        let result = doctor(
            v1_source(),
            &PipelineConfig::default(),
            Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "doctor() with a too-short --duration took {:?} -- it must reject before running \
             the pipeline for the requested (too-short) duration",
            start.elapsed()
        );
    }

    /// Regression: the watchdog must not keep the whole call blocked for
    /// the full requested duration once `listen()` itself has already
    /// returned (EOF on a short file, well before `duration` elapses).
    #[test]
    fn doctor_returns_promptly_when_the_source_ends_well_before_duration() {
        let start = Instant::now();
        // v1_source() is a few seconds of audio at most; --duration asks
        // for far longer than that, so a fixed-duration watchdog with no
        // early-exit signal would block this call for the full 30s.
        let report = doctor(
            v1_source(),
            &PipelineConfig::default(),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "doctor() took {:?} against a source that ended well within its 30s --duration -- \
             the watchdog must exit as soon as listen() returns, not sleep out the full duration",
            start.elapsed()
        );
        assert!(report.duration < Duration::from_secs(10));
    }

    /// Regression: an even-length input must average the two middle
    /// values, not just take the upper-middle element -- `[-2.0, 1.0]`'s
    /// true median is `-0.5`, not `1.0`.
    #[test]
    fn median_averages_the_middle_pair_for_even_length_input() {
        assert_eq!(median(vec![-2.0, 1.0]), Some(-0.5));
        assert_eq!(median(vec![1.0, -2.0]), Some(-0.5));
        assert_eq!(median(vec![1.0, 2.0, 3.0, 4.0]), Some(2.5));
    }

    #[test]
    fn median_returns_the_middle_element_for_odd_length_input() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
    }

    #[test]
    fn median_returns_none_for_empty_input() {
        assert_eq!(median(vec![]), None);
    }
}
