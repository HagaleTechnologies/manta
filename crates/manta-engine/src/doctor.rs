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

/// A run whose median `TrackMeta.snr_2500_db` sits at or below this reads
/// as noise-floor chatter rather than a real signal. NOT a SPEC-defined
/// value -- a doctor-local heuristic, calibrated against the real-hardware
/// runs in docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md, where every
/// track opened against band noise alone (no confirmed spot, an antenna
/// that really was hearing real HF energy elsewhere in the same session)
/// topped out at -0.9 dB. 0.0 dB is a deliberately conservative line above
/// that observed ceiling, not a tight fit to it -- retune if it
/// misclassifies a real run.
pub const NOISE_FLOOR_SNR_DB: f32 = 0.0;

/// `doctor()`'s classification of a run, from cheapest/most-damning signal
/// to most reassuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Verdict {
    /// Not one `TrackMeta` event the whole run -- the detector never even
    /// opened a track. Check the antenna, the gain, and that the tuned
    /// passband actually overlaps where you expect signal.
    NoSignal,
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
                "NO_SIGNAL -- no track ever opened. Check antenna, gain, and that the tuned \
                 passband overlaps where you expect signal."
            }
            Verdict::NoisyNoDecode => {
                "NOISY_NO_DECODE -- tracks opened but every one reads at or below the noise \
                 floor and nothing decoded. Looks like noise, not real CW traffic, right now."
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
    /// Number of `TrackMeta` events seen (one per track, per hop it's
    /// updated -- a proxy for how much detector activity there was, not a
    /// distinct-track count; see `tracks_closed` for that).
    pub track_meta_count: usize,
    /// Number of `TrackClosed` events seen -- the number of distinct
    /// tracks the detector opened and later closed during the run. High
    /// churn (many tracks, each short-lived) over a short window is itself
    /// a signal: real CW holds a track open for as long as the operator is
    /// sending, while noise tends to open and close constantly.
    pub tracks_closed: usize,
    pub snr_db_min: Option<f32>,
    pub snr_db_max: Option<f32>,
    /// Median of every `TrackMeta.snr_2500_db` seen, in dB. `None` if no
    /// `TrackMeta` event occurred (see `Verdict::NoSignal`).
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
        if self.track_meta_count == 0 {
            return Verdict::NoSignal;
        }
        if self.spots_confirmed > 0 {
            return Verdict::Decoding;
        }
        match self.snr_db_median {
            Some(median) if median <= NOISE_FLOOR_SNR_DB => Verdict::NoisyNoDecode,
            _ => Verdict::WeakNoDecode,
        }
    }
}

fn median(mut values: Vec<f32>) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(values[values.len() / 2])
}

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

    let sample_rate_hz = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = stop.clone();
    let start = Instant::now();

    const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);
    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration {
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(SAMPLE_INTERVAL.min(remaining.max(Duration::from_millis(1))));
        }
        stop_watchdog.store(true, Ordering::Relaxed);
    });

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
        duration: start.elapsed(),
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
    }

    #[test]
    fn doctor_reports_no_signal_on_digital_silence() {
        let fs = manta_input::TARGET_RATE_HZ;
        let silence = vec![0.0f32; fs as usize * 2];
        let src: Box<dyn IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(silence, fs)))
                .unwrap(),
        );
        let report = doctor(src, &PipelineConfig::default(), Duration::from_secs(1)).unwrap();
        assert_eq!(report.verdict(), Verdict::NoSignal);
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
}
