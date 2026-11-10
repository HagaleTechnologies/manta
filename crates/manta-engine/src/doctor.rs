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

use crate::{listen_with_observers, ConditioningReport, ListenObservers, PipelineConfig};
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
///
/// JSON: the `summary()` label (`WEAK_NO_DECODE`), the same spelling as
/// the text report (MAN-139).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// No decoder evidence at all the whole run -- no `TrackMeta`, no
    /// `CharDecoded`, no confirmed spot. The detector never even opened a
    /// track. Check the antenna, the gain, and that the tuned passband
    /// actually overlaps where you expect signal.
    NoSignal,
    /// MAN-313: the detector confirmed at least one signal
    /// (`detector_confirmations` is nonzero) but none qualified as keyed
    /// CW, so no track was promoted -- speech, wideband data, or a swept or
    /// steady carrier. The receiver chain works; there is just no CW where
    /// it is tuned.
    NotCw,
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
            Verdict::NotCw => {
                "NOT_CW -- the detector saw signals but none was keyed CW (speech, data, or a \
                 swept or steady carrier). The receiver chain works; tune to where CW activity \
                 is, or set [detector] non_cw_filter = false to decode everything."
            }
            Verdict::ActivityNoSnr => {
                "ACTIVITY_NO_SNR -- the detector promoted a candidate (possibly closing, possibly \
                 decoding characters too) but no SNR was ever measured before this run ended, and \
                 nothing confirmed a spot. Can't yet say noise vs. real signal -- try a longer \
                 --duration."
            }
            Verdict::NoisyNoDecode => {
                "NOISY_NO_DECODE -- tracks opened but every one reads at or below the noise \
                 floor and nothing validated into a confirmed spot. Looks like noise, not real \
                 CW traffic, right now (characters may still have been decoded off the noise \
                 floor -- the decode line counts them)."
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
///
/// `doctor --json` prints it with camelCase keys, `durationS` in seconds
/// and the SNR as `snrMin`/`snrMedian`/`snrMax` (MAN-139,
/// docs/CLI-JSON.md).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    /// `IqSource::sample_rate()`, as actually negotiated (SoapySDR and
    /// similar sources round to the nearest achievable rate; this is truth,
    /// not the ask).
    pub sample_rate_hz: f64,
    /// `IqSource::center_freq_hz()`, as actually negotiated.
    pub center_freq_hz: f64,
    /// How much SOURCE TIME the run actually analyzed -- derived from
    /// samples consumed (`SampleBoundedSource`), not wall-clock elapsed
    /// time. An unpaced file source (`--source`) can process audio far
    /// faster (or, under load, slower) than 1:1, so wall-clock time and
    /// source time can differ substantially; a live source is naturally
    /// paced to real time, so the two coincide there. `durationS` in JSON,
    /// a number of seconds (MAN-139).
    #[serde(rename = "durationS", serialize_with = "duration_secs")]
    pub duration: Duration,
    /// Number of `TrackPromoted` events seen -- the detector's own ground
    /// truth for "found a keyed CW signal," independent of whether the
    /// decoder ever got far enough to produce a `TrackMeta`/`CharDecoded`/
    /// `TrackClosed` afterward. Since MAN-313 (with `[detector]
    /// non_cw_filter` on, the default) a track is promoted only once it
    /// shows CW keying, so speech, data and swept or steady carriers are
    /// not counted here; `detector_confirmations` counts them. This, not
    /// any decode-timing proxy, is what `verdict()`'s `NoSignal`/`NotCw`
    /// checks key off (see
    /// docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md).
    pub tracks_promoted: usize,
    /// MAN-313: every CANDIDATE -> ACTIVE confirmation the detector made
    /// during the run, whether or not the track qualified as keyed CW
    /// (`TrackManager::confirmations`, read through
    /// `ListenObservers::detector_confirmations`). Nonzero with
    /// `tracks_promoted == 0` means signals were present but none was
    /// keyed CW (`Verdict::NotCw`).
    pub detector_confirmations: usize,
    /// Number of `TrackMeta` events seen (one per track, per hop it's
    /// updated -- a proxy for how much decoder activity there was, not a
    /// distinct-track count).
    pub track_meta_count: usize,
    /// Number of `TrackClosed` events seen. `TrackManager` only emits
    /// `TrackClosed` for a track that produced at least one other event
    /// before closing (`has_emitted`, MAN-19, `track.rs`), or that a
    /// merged-away track named as its survivor (MAN-174) -- an unpromoted
    /// CANDIDATE, or a promoted track merged/evicted before its first
    /// `process_hops` pass, closes with no event at all. So this counts
    /// *eventful* tracks that closed, not every detector candidate opened
    /// during the run -- use `tracks_promoted` for that.
    pub tracks_closed: usize,
    /// Lowest `TrackMeta.snr_2500_db` seen, in dB; `snrMin` in JSON.
    #[serde(rename = "snrMin")]
    pub snr_db_min: Option<f32>,
    /// Median of every `TrackMeta.snr_2500_db` seen, in dB. `None` if no
    /// `TrackMeta` event occurred. `snrMedian` in JSON.
    #[serde(rename = "snrMedian")]
    pub snr_db_median: Option<f32>,
    /// Highest `TrackMeta.snr_2500_db` seen, in dB; `snrMax` in JSON.
    #[serde(rename = "snrMax")]
    pub snr_db_max: Option<f32>,
    /// Total `CharDecoded` events across every track.
    pub chars_decoded: usize,
    /// Distinct decoded characters seen (via `Glyph::text_char`). Real CW
    /// text uses a broad alphabet; a noise-floor track tends to collapse
    /// onto one or two characters (`E`/`T`, the shortest morse elements)
    /// repeated at length.
    pub distinct_chars: usize,
    /// Confirmed, fully-validated spots -- the strictest signal.
    pub spots_confirmed: usize,
    /// MAN-234: input samples replaced with silence because a component
    /// was NaN, infinite or beyond `MAX_SAMPLE_MAGNITUDE` (SPEC §1.3 step
    /// 0). `badSamples` in `doctor --json`, omitted when 0. Not an input to `verdict()`:
    /// the replaced samples are silence, so the verdict judges what remains.
    #[serde(skip_serializing_if = "crate::is_zero")]
    pub bad_samples: u64,
    /// MAN-495: what the input-conditioning chain observed, every stage
    /// doctor did not keep applied running as `observe`. `inputConditioning`
    /// in `doctor --json`, always present when a receiver ran (doctor
    /// forces the stages on so diagnostics exist even on an all-off
    /// config), absent in checks-only output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_conditioning: Option<ConditioningReport>,
    /// MAN-530: samples the source's capture queue dropped during THIS run
    /// (startup calibration included), read from
    /// `IqSource::health_counters` after the run ends -- live rig audio's
    /// capture-ring overruns, or another receiver's queue evictions.
    /// `queueDroppedSamples` in `doctor --json`, omitted when 0; a source
    /// with no health counters reads as 0. Not a daemon's historical
    /// total, and not an input to `verdict()`.
    #[serde(skip_serializing_if = "crate::is_zero")]
    pub queue_dropped_samples: u64,
    /// MAN-530: input gap events the source counted during this run
    /// (`InputHealthCounters::gaps_detected`): a contiguous audio overrun,
    /// a SoapySDR overflow or a packet-stream loss. Source-generic, so not
    /// labelled an audio overrun. `inputGapsDetected` in `doctor --json`,
    /// omitted when 0.
    #[serde(skip_serializing_if = "crate::is_zero")]
    pub input_gaps_detected: u64,
}

/// `DoctorReport::duration` as a number of seconds (MAN-139), the value the
/// text report prints as `observed for 10.0s`.
fn duration_secs<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

impl DoctorReport {
    pub fn verdict(&self) -> Verdict {
        if self.spots_confirmed > 0 {
            return Verdict::Decoding;
        }
        // `tracks_promoted` is the detector's own ground truth, not a
        // decode-timing proxy (TrackMeta/CharDecoded/TrackClosed all
        // depend on the decoder getting further than mere promotion --
        // e.g. TrackMeta needs ~1s of decoder init on top of the
        // promotion latency -- ~0.07s since MAN-115's 25 ms warmup, ~2.05s
        // before it or with `warmup_ms = 2000` -- which a short --duration
        // run can end before reaching at all). Promotion alone
        // is what "the detector found a candidate" means.
        if self.tracks_promoted == 0 {
            // MAN-313: no keyed CW track, but the detector did confirm
            // signals -- the receiver chain works, the band just holds no
            // CW (speech, data, a swept or steady carrier).
            if self.detector_confirmations > 0 {
                return Verdict::NotCw;
            }
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
    if values.len().is_multiple_of(2) {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

/// Minimum accepted `--duration`. Set when `listen()`'s fixed ~2s startup
/// calibration read loop (`CALIBRATION_SECONDS`, private to `listen.rs`)
/// ignored `stop` and read that whole window regardless, so a shorter
/// requested duration silently overran it while still reporting the
/// shorter number back. Since MAN-122 review round 7 that loop checks
/// `stop` before each read and analyzes only what it has read, so a
/// shorter duration ends inside the window instead; the floor is unchanged
/// and still keeps a run past it. Comfortably above that 2s floor, not a
/// tight fit to it. Kept local to doctor rather than plumbed into
/// `listen()` itself, since ordinary `manta listen` has no duration bound
/// to enforce in the first place.
///
/// This margin assumes the pipeline processes that 2s calibration buffer
/// in at most its own wall-clock duration (real-time or faster) -- the
/// project's own CPU-budget requirement (CLAUDE.md: full 192 kS/s passband
/// within one Raspberry Pi 4 core), tracked and benched separately
/// (ROADMAP M2's CPU-budget bench). A machine running meaningfully slower
/// than that (round-5 review: roughly below 0.5x real-time) could still
/// overrun this bound while processing that buffer, which `stop` does not
/// interrupt once its reads have ended, or while a single read is still in
/// flight -- the same underlying gap as `listen()`'s blocking-read
/// interruptibility already noted as a deferred, shared, cross-cutting
/// limitation (not something to fix inside doctor alone).
pub const MIN_DURATION: Duration = Duration::from_secs(3);

/// Maximum accepted `--duration`. `doctor()` accumulates every `TrackMeta`
/// SNR sample in memory for the whole run to compute `snr_db_median` at the
/// end -- on a noisy full passband with sustained track churn, an
/// unbounded duration means unbounded memory (tens of millions of floats
/// over a 24h+ run). `doctor` is a bounded-duration health check, not a
/// long-running monitor (that's `manta run --config`'s
/// Prometheus metrics), so a generous but real ceiling is the right fix,
/// not a fancier streaming-quantile estimator for a tool that was never
/// meant to run for hours in the first place.
pub const MAX_DURATION: Duration = Duration::from_secs(3600);

/// Wraps a source, forcing EOF (`read()` returns `Ok(0)`) once
/// `max_samples` samples have been delivered -- see the doc comment where
/// it's constructed in `doctor()` for why this is needed. `samples_read`
/// is shared (not owned) so `doctor()` can read the final count after
/// `listen()` consumes and eventually drops the source -- needed to
/// report actual SOURCE TIME consumed (round-8 review finding: the
/// report's `duration` still reflected wall-clock elapsed time even
/// after this wrapper started correctly bounding what gets analyzed, so
/// an unpaced file source could analyze 30s of audio in 2s of wall time
/// and the report would claim only 2.0s was observed).
struct SampleBoundedSource {
    inner: Box<dyn IqSource>,
    max_samples: u64,
    samples_read: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl IqSource for SampleBoundedSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        use std::sync::atomic::Ordering;
        let already_read = self.samples_read.load(Ordering::Relaxed);
        if already_read >= self.max_samples {
            return Ok(0);
        }
        // Saturate, don't truncate: on a 32-bit target a long, high-rate
        // run's `remaining` count can exceed `usize::MAX` (round-7 review
        // finding -- e.g. 1.536 MS/s for 3600s is ~5.53 billion samples).
        // A bare `as usize` cast wraps, and if it ever wraps to exactly
        // 0 the read is called with an empty buffer, which returns Ok(0)
        // -- `listen()` reads that as EOF, ending the whole run early.
        // Saturating first means `cap` is only ever bounded by `buf.len()`
        // itself (already a real usize), never spuriously zero.
        let remaining = usize::try_from(self.max_samples - already_read).unwrap_or(usize::MAX);
        let cap = remaining.min(buf.len());
        let n = self.inner.read(&mut buf[..cap])?;
        self.samples_read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    // MAN-196 (round-1 review on PR #300): forward the inner source's
    // bands, like every other wrapper does. With the trait defaults,
    // `doctor --capture-rate-hz` ran `listen()` on a `DecimatingSource`
    // with the trusted-band spawn veto silently disabled (`None`) and with
    // the static-crash live set / reported channel levels judged over the
    // un-narrowed +/-fs_out/2 band -- a different detector path from
    // `run`/`soak`, able to count transition-band or folded tracks those
    // commands reject.
    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }

    // MAN-305: forward the gap hooks, as `listen()` reads them. With the
    // trait defaults `doctor` would splice an outage it was told about and
    // time spots after a short loss early by the lost samples; MAN-202 adds
    // the unknown-length loss, which would otherwise decode as timing.
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    fn take_missing_samples(&mut self) -> Option<u64> {
        self.inner.take_missing_samples()
    }

    fn take_unknown_gap(&mut self) -> bool {
        self.inner.take_unknown_gap()
    }

    /// MAN-201: forwarded as the gap hooks are.
    fn take_stalled(&mut self) -> bool {
        self.inner.take_stalled()
    }

    /// MAN-273: forwarded with `trusted_band_hz`.
    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }
}

/// How much input doctor's floor pre-pass measures: the first
/// `FLOOR_TAP_SECONDS` of source time after leading digital silence.
/// This lets the per-channel floor histograms' lower quartile settle.
/// The measurement cannot outlast the run.
const FLOOR_TAP_SECONDS: f64 = 1.0;

/// MAN-495: the reference `dc.overChannelNoiseDb` needs. A DC offset is
/// only worth naming when it stands over the channel noise floor, and the
/// DC estimate alone (dBFS) says nothing about that -- a quiet receiver
/// and a loud one carry the same offset at very different over-floor
/// levels. `manta check` already summarizes per-channel floors
/// (`check.rs`); doctor measures the same thing through a tee so the
/// decode itself is untouched. After leading digital silence, samples
/// are also fed to a raw `Channelizer::new` + `FloorBank` for up to
/// `FLOOR_TAP_SECONDS` of source time, then the tap goes quiet. The
/// channelizer is raw on purpose (D9, like `check`): the floor is a
/// property of the receiver, not of any conditioning doctor is judging.
struct FloorTapShared {
    ch: manta_dsp::channelizer::Channelizer,
    floor: manta_dsp::floor::FloorBank,
    db: Vec<f64>,
    fed: u64,
    any_hop: bool,
}

/// The tap's read-side half; the shared state is kept so `doctor()` can
/// read the measurement after `listen()` has consumed and dropped the
/// source itself (the same shape `SampleBoundedSource`'s `samples_read`
/// counter takes).
struct FloorTapSource {
    inner: Box<dyn IqSource>,
    state: Arc<Mutex<FloorTapShared>>,
    max_samples: u64,
}

impl FloorTapSource {
    /// The tap for `src` at its own negotiated rate.
    fn new(src: Box<dyn IqSource>) -> Result<(Self, Arc<Mutex<FloorTapShared>>)> {
        let fs = src.sample_rate();
        let center = src.center_freq_hz();
        let ch = manta_dsp::channelizer::Channelizer::new(fs, center)
            .map_err(anyhow::Error::msg)
            .map_err(|e| e.context("floor tap channelizer"))?;
        let n = ch.n_channels();
        let state = Arc::new(Mutex::new(FloorTapShared {
            floor: manta_dsp::floor::FloorBank::new(n),
            db: vec![0.0; n],
            ch,
            fed: 0,
            any_hop: false,
        }));
        Ok((
            FloorTapSource {
                inner: src,
                state: state.clone(),
                max_samples: (fs * FLOOR_TAP_SECONDS).round() as u64,
            },
            state,
        ))
    }
}

/// The tap's median floor over the eligible passband channels, in dBFS,
/// once it saw at least one hop. MAN-203 D8: channels within two of the
/// centre and within two of either Nyquist edge are excluded -- exactly
/// where a DC offset (or its images) lifts the floor the measurement
/// exists to compare against.
fn median_floor_db(state: &FloorTapShared, passband: [f64; 2]) -> Option<f64> {
    if !state.any_hop {
        return None;
    }
    let n = state.ch.n_channels() as i64;
    let center = state.ch.channel_freq_hz(0);
    let mut values: Vec<f64> = (0..state.ch.n_channels())
        .filter(|&k| {
            let signed = ((k as i64 + n / 2) % n) - n / 2;
            if signed.abs() <= 2 || signed.abs() >= n / 2 - 2 {
                return false;
            }
            let offset = state.ch.channel_freq_hz(k) - center;
            offset >= passband[0] && offset < passband[1]
        })
        .map(|k| state.floor.channel_floor_db(k))
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

impl IqSource for FloorTapSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        let state = &mut *self.state.lock().unwrap_or_else(|e| e.into_inner());
        // A receiver can deliver digital silence before it starts. Do not
        // spend the reference budget until actual input arrives. The tee
        // still returns all samples unchanged to the listening pipeline.
        let start = if state.fed == 0 {
            buf[..n]
                .iter()
                .position(|s| s.re != 0.0 || s.im != 0.0)
                .unwrap_or(n)
        } else {
            0
        };
        let take = self
            .max_samples
            .saturating_sub(state.fed)
            .min((n - start) as u64) as usize;
        if take > 0 {
            state.fed += take as u64;
            for hop in state.ch.process(&buf[start..start + take]) {
                for (value, &power) in state.db.iter_mut().zip(&hop.power) {
                    *value = manta_dsp::channelizer::power_db(power);
                }
                state.floor.update(&state.db);
                state.any_hop = true;
            }
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    fn take_missing_samples(&mut self) -> Option<u64> {
        self.inner.take_missing_samples()
    }

    fn take_unknown_gap(&mut self) -> bool {
        self.inner.take_unknown_gap()
    }

    /// MAN-201: forwarded as the gap hooks are.
    fn take_stalled(&mut self) -> bool {
        self.inner.take_stalled()
    }

    /// MAN-273: forwarded with `trusted_band_hz`.
    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }
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
    if duration < MIN_DURATION {
        anyhow::bail!(
            "--duration must be at least {}s -- listen() spends its first ~2s of audio filling \
             a fixed startup calibration window, so a shorter run would end inside it",
            MIN_DURATION.as_secs()
        );
    }
    if duration > MAX_DURATION {
        anyhow::bail!(
            "--duration must be at most {}s -- doctor() accumulates every TrackMeta SNR sample \
             in memory for the run's duration, so an unbounded --duration means unbounded \
             memory; for a long-running check, use `manta run --config` instead",
            MAX_DURATION.as_secs()
        );
    }

    let sample_rate_hz = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();
    let (lo, hi) = src.rf_passband_hz();
    // Clamped to Nyquist the way `check.rs` clamps its own floor summary,
    // so `median_floor_db`'s eligibility filter can compare against
    // channel offsets, which never leave +/-fs/2.
    let passband = [lo.max(-sample_rate_hz / 2.0), hi.min(sample_rate_hz / 2.0)];
    // MAN-495: doctor always observes every conditioning stage that is
    // not set to `apply` -- an `off` stage becomes `observe` so diagnostics
    // exist even on a default config, and an `apply` stage stays applied
    // because the operator asked for the correction (observe never writes
    // a sample, so a forced-observe run decodes byte-identically to an
    // unconditioned one).
    let cfg = PipelineConfig {
        conditioning: cfg.conditioning.observing(),
        ..cfg.clone()
    };
    // A file-replay source (`--source`) delivers samples as fast as
    // processing allows, not paced to real time -- unlike a live source,
    // which naturally can't outrun its own sample clock. Left unbounded,
    // doctor's watchdog only caps WALL-CLOCK time, so a fast machine could
    // analyze the entire file regardless of --duration and report a
    // verdict from content far outside the requested window while still
    // claiming only a few seconds were "observed" (round-6 review
    // finding). Bounding samples read, not just elapsed time, fixes this
    // for every source type -- a live source is unaffected (it can never
    // exceed real-time delivery in the first place), so this is a no-op
    // safety net there and the actual fix for file replay.
    let max_samples = (sample_rate_hz * duration.as_secs_f64()).round() as u64;
    // MAN-530: held before the wrappers below take the source, so the
    // report can read this run's final loss totals after `listen` returns
    // (the wrappers need not forward `health_counters` for doctor's sake).
    let health = src.health_counters();
    let samples_read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let src: Box<dyn IqSource> = Box::new(SampleBoundedSource {
        inner: src,
        max_samples,
        samples_read: samples_read.clone(),
    });
    // MAN-495: the floor tee beside the sample bound, and the conditioning
    // report observer the run publishes into (pre-seeded with the forced
    // config's modes, so the report reads `observe`/`apply` for what
    // doctor itself ran).
    let (floor_src, floor_tap) = FloorTapSource::new(src)?;
    let src: Box<dyn IqSource> = Box::new(floor_src);
    let conditioning_slot = Arc::new(Mutex::new(ConditioningReport::for_config(
        &cfg.conditioning,
    )));

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
    let confirmations = Arc::new(AtomicU64::new(0));

    let bad_samples = Arc::new(AtomicU64::new(0));
    let result = listen_with_observers(
        src,
        &cfg,
        stop.clone(),
        ListenObservers {
            detector_confirmations: Some(confirmations.clone()),
            bad_samples: Some(bad_samples.clone()),
            conditioning_report: Some(conditioning_slot.clone()),
            ..ListenObservers::default()
        },
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
        |_n| {},
    );
    listen_done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    result?;

    // SOURCE time consumed, not wall-clock elapsed: an unpaced file
    // source (`--source`) can process far more (or less) audio per
    // wall-clock second than 1:1, so `start.elapsed()` doesn't describe
    // what was actually analyzed (round-8 review finding). A live source
    // is naturally paced to real time, so this is equivalent to
    // wall-clock there anyway.
    let observed_duration = Duration::from_secs_f64(
        (samples_read.load(std::sync::atomic::Ordering::Relaxed) as f64 / sample_rate_hz).max(0.0),
    );

    let snr_db_min = snrs.iter().copied().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.min(v)))
    });
    let snr_db_max = snrs.iter().copied().fold(None, |acc: Option<f32>, v| {
        Some(acc.map_or(v, |a| a.max(v)))
    });
    let snr_db_median = median(snrs);

    // MAN-495: the observer holds the run's folded conditioning report;
    // `overChannelNoiseDb` is doctor's own measurement, filled in from the
    // floor tee once the run gives it a DC level to compare (both sides
    // are dBFS, so their difference is the over-floor dB the report and
    // the `input:` DC line print).
    let mut input_conditioning = Some(
        conditioning_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    );
    if let Some(report) = input_conditioning.as_mut() {
        if let (Some(level), Some(floor)) = (
            report.dc.level_dbfs,
            median_floor_db(
                &floor_tap.lock().unwrap_or_else(|e| e.into_inner()),
                passband,
            ),
        ) {
            report.dc.over_channel_noise_db = Some(level - floor);
        }
    }

    Ok(DoctorReport {
        sample_rate_hz,
        center_freq_hz,
        duration: observed_duration,
        tracks_promoted,
        detector_confirmations: usize::try_from(confirmations.load(Ordering::Relaxed))
            .unwrap_or(usize::MAX),
        track_meta_count,
        tracks_closed,
        snr_db_min,
        snr_db_max,
        snr_db_median,
        chars_decoded,
        distinct_chars: distinct_chars.len(),
        spots_confirmed,
        bad_samples: bad_samples.load(Ordering::Relaxed),
        input_conditioning,
        queue_dropped_samples: health.as_ref().map_or(0, |h| h.queue_dropped_samples()),
        input_gaps_detected: health.as_ref().map_or(0, |h| h.gaps_detected()),
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

    /// Regression (round-6 review): a file-replay source delivers samples
    /// as fast as processing allows, not paced to real time -- unbounded,
    /// doctor could analyze an entire long file regardless of --duration.
    /// `SampleBoundedSource` must force EOF at exactly `max_samples`, even
    /// though the wrapped source has far more available.
    #[test]
    fn sample_bounded_source_forces_eof_at_max_samples() {
        let inner_samples = vec![Complex32::new(1.0, 0.0); 1000];
        let mut src = SampleBoundedSource {
            inner: Box::new(RawIqSource {
                samples: inner_samples,
                cursor: 0,
                fs: 48_000.0,
                center_freq_hz: 0.0,
            }),
            max_samples: 100,
            samples_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let mut buf = vec![Complex32::new(0.0, 0.0); 1000];
        let n = src.read(&mut buf).unwrap();
        assert_eq!(n, 100, "must cap the read to max_samples in one call");
        let n2 = src.read(&mut buf).unwrap();
        assert_eq!(
            n2, 0,
            "must report EOF once max_samples have been delivered, even though the wrapped \
             source has 900 more samples available"
        );
        assert_eq!(
            src.samples_read.load(std::sync::atomic::Ordering::Relaxed),
            100,
            "the shared counter doctor() reads after listen() returns must reflect exactly what \
             was delivered"
        );
    }

    /// Regression (MAN-196 round-1 review on PR #300): `SampleBoundedSource`
    /// must forward the inner source's `trusted_band_hz`/`rf_passband_hz`.
    /// The trait defaults (`None` / +/-sample_rate/2) are wrong for a
    /// wrapped `DecimatingSource`, whose veto band and narrowed passband
    /// would otherwise be silently dropped, running `listen()` on a
    /// different detector path from `run`/`soak`.
    #[test]
    fn sample_bounded_source_forwards_the_inner_bands() {
        struct BandedSource;
        impl IqSource for BandedSource {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn rf_passband_hz(&self) -> (f64, f64) {
                (-21_724.8, 21_724.8)
            }
            fn trusted_band_hz(&self) -> Option<(f64, f64)> {
                Some((-21_724.8, 21_724.8))
            }
            fn read(&mut self, _buf: &mut [Complex32]) -> Result<usize> {
                Ok(0)
            }
        }
        let src = SampleBoundedSource {
            inner: Box::new(BandedSource),
            max_samples: 10,
            samples_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        assert_eq!(
            src.trusted_band_hz(),
            Some((-21_724.8, 21_724.8)),
            "the wrapped source's trusted band must reach listen()'s spawn veto"
        );
        assert_eq!(
            src.rf_passband_hz(),
            (-21_724.8, 21_724.8),
            "the wrapped source's narrowed passband must reach the crash live set and reported \
             channel levels, not the default +/-fs_out/2"
        );
    }

    /// MAN-305: `SampleBoundedSource` hands the inner source's outage and
    /// spliced loss to `listen()`, each once; MAN-202: and its
    /// unknown-length loss.
    #[test]
    fn sample_bounded_source_forwards_every_gap_report() {
        struct GapReportingSource {
            gap: Option<u64>,
            missing: Option<u64>,
            unknown: bool,
        }
        impl IqSource for GapReportingSource {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                Ok(buf.len())
            }
            fn take_discontinuity(&mut self) -> Option<u64> {
                self.gap.take()
            }
            fn take_missing_samples(&mut self) -> Option<u64> {
                self.missing.take()
            }
            fn take_unknown_gap(&mut self) -> bool {
                std::mem::take(&mut self.unknown)
            }
        }
        let mut src = SampleBoundedSource {
            inner: Box::new(GapReportingSource {
                gap: Some(48_000),
                missing: Some(11_900),
                unknown: true,
            }),
            max_samples: 1_000,
            samples_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let mut buf = vec![Complex32::new(0.0, 0.0); 100];
        assert_eq!(src.read(&mut buf).unwrap(), 100);
        assert_eq!(src.take_discontinuity(), Some(48_000));
        assert_eq!(src.take_missing_samples(), Some(11_900));
        assert!(src.take_unknown_gap());
        assert_eq!(src.take_discontinuity(), None);
        assert_eq!(src.take_missing_samples(), None);
        assert!(!src.take_unknown_gap());
    }

    #[test]
    fn doctor_reports_decoding_on_a_clean_golden_signal() {
        // 60s, not 5s: V1's looped "CQ CQ DE W1AW W1AW K" at 20 WPM needs
        // real audio-time for the repetition gate to confirm a spot, and
        // SampleBoundedSource (round-6 review fix) now genuinely bounds
        // v1_source() (a 120s vector) to --duration worth of SOURCE TIME,
        // not just wall-clock time -- 5s was never really enough here; it
        // only looked sufficient before that fix, when doctor silently
        // analyzed the entire 120s file regardless of --duration.
        let report = doctor(
            v1_source(),
            &PipelineConfig::default(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(report.verdict(), Verdict::Decoding);
        assert!(report.spots_confirmed > 0);
        assert!(report.track_meta_count > 0);
        assert!(report.tracks_promoted > 0);
    }

    /// MAN-234 scenario 3: one finite `f32::MAX` sample at 1 ms latched
    /// the floor/gate averages at NaN, so no track ever spawned and the
    /// verdict read `NO_SIGNAL`. The guard replaces it with silence.
    #[test]
    fn doctor_on_a_finite_overflow_sample_is_not_no_signal() {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 20.0,
            ..manta_testkit::vectors::v1()
        };
        let mut samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        samples[96].re = f32::MAX;
        let src: Box<dyn IqSource> = Box::new(RawIqSource {
            samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let report = doctor(src, &PipelineConfig::default(), Duration::from_secs(60)).unwrap();
        assert_ne!(report.verdict(), Verdict::NoSignal, "{report:?}");
        assert_eq!(report.bad_samples, 1);
        assert!(report.tracks_promoted >= 1, "{report:?}");
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
            detector_confirmations: 0,
            track_meta_count: 5,
            tracks_closed: 3,
            snr_db_min: Some(-8.0),
            snr_db_max: Some(-1.0),
            snr_db_median: Some(-6.0),
            chars_decoded: 50,
            distinct_chars: 2,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
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
            detector_confirmations: 0,
            track_meta_count: 5,
            tracks_closed: 1,
            snr_db_min: Some(5.0),
            snr_db_max: Some(15.0),
            snr_db_median: Some(10.0),
            chars_decoded: 200,
            distinct_chars: 20,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
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
            detector_confirmations: 0,
            track_meta_count: 20,
            tracks_closed: 15,
            snr_db_min: Some(-8.0),
            snr_db_max: Some(12.0),
            snr_db_median: Some(-6.0),
            chars_decoded: 300,
            distinct_chars: 3,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
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
            detector_confirmations: 0,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 4,
            distinct_chars: 3,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
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
            detector_confirmations: 0,
            track_meta_count: 0,
            tracks_closed: 1,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
        };
        assert_eq!(report.verdict(), Verdict::ActivityNoSnr);
    }

    /// Regression (round-4 review finding): a real signal rising just
    /// after the detector warmup (~2s before MAN-115, 25 ms now) or near
    /// the end of a short run can reach `finish()` having only
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
            detector_confirmations: 0,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
        };
        assert_eq!(report.verdict(), Verdict::ActivityNoSnr);
    }

    const ALL_VERDICTS: [Verdict; 6] = [
        Verdict::NoSignal,
        Verdict::NotCw,
        Verdict::ActivityNoSnr,
        Verdict::NoisyNoDecode,
        Verdict::WeakNoDecode,
        Verdict::Decoding,
    ];

    /// MAN-139: `doctor --json`'s verdict is the text report's label.
    #[test]
    fn verdict_json_is_the_summary_label() {
        for v in ALL_VERDICTS {
            let label = v.summary().split(" -- ").next().unwrap();
            assert_eq!(serde_json::to_value(v).unwrap(), label);
        }
    }

    /// MAN-139: no verdict sentence names a `doctor --json` key, in its
    /// camelCase or its old snake_case spelling (`non_cw_filter`, a TOML
    /// key the operator sets, is not one).
    #[test]
    fn no_summary_names_a_json_key() {
        let mut report = no_promotion_report(1);
        report.bad_samples = 1;
        let mut names = Vec::new();
        for key in crate::json_keys_in_order(&serde_json::to_string(&report).unwrap()) {
            let mut snake = String::new();
            for c in key.chars() {
                if c.is_ascii_uppercase() {
                    snake.push('_');
                }
                snake.push(c.to_ascii_lowercase());
            }
            names.push(key);
            names.push(snake);
        }
        names.extend(
            [
                "checksStatus",
                "checks_status",
                "snr_db_min",
                "snr_db_median",
                "snr_db_max",
            ]
            .map(String::from),
        );
        for v in ALL_VERDICTS {
            let summary = v.summary();
            for word in summary.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                assert!(!names.iter().any(|n| n == word), "{word:?} in {summary}");
            }
            assert!(!summary.contains("chars_decoded"), "{summary}");
            assert!(!summary.contains("charsDecoded"), "{summary}");
        }
    }

    /// MAN-139: `doctor --json`'s report keys are camelCase in declaration
    /// order, the SNR reads min/median/max like the text line, and the
    /// duration is a number of seconds.
    #[test]
    fn doctor_report_json_is_camel_case_with_seconds() {
        let mut report = no_promotion_report(1);
        report.duration = Duration::from_millis(2500);
        report.snr_db_min = Some(1.0);
        report.snr_db_median = Some(2.0);
        report.snr_db_max = Some(3.0);
        report.bad_samples = 1;
        let s = serde_json::to_string(&report).unwrap();
        assert!(s.starts_with(r#"{"sampleRateHz":"#), "{s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            crate::json_keys_in_order(&s),
            [
                "sampleRateHz",
                "centerFreqHz",
                "durationS",
                "tracksPromoted",
                "detectorConfirmations",
                "trackMetaCount",
                "tracksClosed",
                "snrMin",
                "snrMedian",
                "snrMax",
                "charsDecoded",
                "distinctChars",
                "spotsConfirmed",
                "badSamples",
            ]
        );
        assert_eq!(v["durationS"], 2.5);
    }

    fn no_promotion_report(detector_confirmations: usize) -> DoctorReport {
        DoctorReport {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 14_000_000.0,
            duration: Duration::from_secs(10),
            tracks_promoted: 0,
            detector_confirmations,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_max: None,
            snr_db_median: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
            bad_samples: 0,
            input_conditioning: None,
            queue_dropped_samples: 0,
            input_gaps_detected: 0,
        }
    }

    /// MAN-313: signals confirmed but none keyed CW (speech, data, a swept
    /// or steady carrier) is not "no signal" -- the receiver chain works.
    #[test]
    fn verdict_is_not_cw_when_confirmations_but_no_promotions() {
        let report = no_promotion_report(14);
        assert_eq!(report.verdict(), Verdict::NotCw);
        assert!(
            report.verdict().summary().starts_with(
                "NOT_CW -- the detector saw signals but none was keyed CW (speech, data, or a \
                 swept or steady carrier). The receiver chain works; tune to where CW activity \
                 is, or set [detector] non_cw_filter = false to decode everything."
            ),
            "{}",
            report.verdict().summary()
        );
    }

    #[test]
    fn verdict_stays_no_signal_with_zero_confirmations() {
        assert_eq!(no_promotion_report(0).verdict(), Verdict::NoSignal);
    }

    #[test]
    fn verdict_prefers_decoding_over_not_cw() {
        let report = DoctorReport {
            spots_confirmed: 1,
            ..no_promotion_report(5)
        };
        assert_eq!(report.verdict(), Verdict::Decoding);
        let report = DoctorReport {
            tracks_promoted: 1,
            ..no_promotion_report(5)
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
        // D11/MAN-77: the advice this error gives has to name the canonical
        // daemon verb. Recommending the deprecated `listen --server-config`
        // spelling sends the operator straight into two deprecation warnings
        // (Codex P2 on PR #125).
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("manta run --config"),
            "the too-long --duration error must point at the canonical daemon command, got: {msg}"
        );
        assert!(
            !msg.contains("--server-config"),
            "the too-long --duration error must not recommend the deprecated flag, got: {msg}"
        );
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
        // v1_source() is actually 120s of audio (vectors::v1()'s own
        // duration_s) -- delivered unpaced (as fast as processing allows,
        // not paced to real time), so even a --duration far shorter than
        // 120s finishes near-instantly in WALL-CLOCK terms. SampleBoundedSource
        // still caps the SOURCE time consumed at the requested 30s (round-6
        // fix), and report.duration now reflects that source time, not wall
        // time (round-8 fix) -- so the two assertions below check two
        // different things on purpose: this call must be fast (wall clock),
        // but must still faithfully report having analyzed close to the
        // full requested 30s of audio content.
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
        assert!(
            report.duration >= Duration::from_secs(29)
                && report.duration <= Duration::from_secs(30),
            "report.duration should reflect ~30s of SOURCE time consumed (SampleBoundedSource's \
             own bound), not the near-instant wall-clock time this unpaced replay actually took, \
             got {:?}",
            report.duration
        );
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

    /// A noise fixture at the testkit rate with a constant receiver DC
    /// offset added, for the conditioning doctor tests.
    fn dc_offset_source(dc: (f32, f32), seconds: f64) -> Box<dyn IqSource> {
        use manta_testkit::sensitivity::{render_noise, CENTER_FREQ_HZ, SAMPLE_RATE_HZ};
        let noise = render_noise(seconds, 203, 1).expect("render the noise");
        let samples: Vec<Complex32> = noise
            .into_iter()
            .map(|s| s + Complex32::new(dc.0, dc.1))
            .collect();
        Box::new(RawIqSource {
            samples,
            cursor: 0,
            fs: SAMPLE_RATE_HZ,
            center_freq_hz: CENTER_FREQ_HZ,
        })
    }

    /// Runs the same pipeline doctor runs, but with every conditioning
    /// stage off (a plain `listen_with_observers` over exactly the samples
    /// doctor's `SampleBoundedSource` would deliver), counting the events
    /// doctor tabulates. Doctor's forced-observe stages never write a
    /// sample, so its counts must equal these.
    fn unconditioned_counts(samples: &[Complex32], fs: f64, center: f64) -> (usize, usize) {
        let (mut promoted, mut chars) = (0usize, 0usize);
        listen_with_observers(
            Box::new(RawIqSource {
                samples: samples.to_vec(),
                cursor: 0,
                fs,
                center_freq_hz: center,
            }),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers::default(),
            |ev| match ev {
                DecoderEvent::TrackPromoted { .. } => promoted += 1,
                DecoderEvent::CharDecoded { .. } => chars += 1,
                _ => {}
            },
            |_| {},
            |_| {},
        )
        .unwrap();
        (promoted, chars)
    }

    #[test]
    fn floor_tap_waits_for_noise_after_leading_digital_silence() {
        use manta_testkit::sensitivity::{render_noise, CENTER_FREQ_HZ, SAMPLE_RATE_HZ};
        let noise = render_noise(2.0, 203, 1).unwrap();
        let measure = |silence: usize| {
            let samples = std::iter::repeat_n(Complex32::new(0.0, 0.0), silence)
                .chain(noise.iter().copied())
                .collect();
            let (mut tap, shared) = FloorTapSource::new(Box::new(RawIqSource {
                samples,
                cursor: 0,
                fs: SAMPLE_RATE_HZ,
                center_freq_hz: CENTER_FREQ_HZ,
            }))
            .unwrap();
            let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
            while tap.read(&mut buf).unwrap() != 0 {}
            let state = shared.lock().unwrap();
            median_floor_db(&state, [-SAMPLE_RATE_HZ / 2.0, SAMPLE_RATE_HZ / 2.0]).unwrap()
        };
        let immediate = measure(0);
        let delayed = measure((SAMPLE_RATE_HZ * 1.5) as usize);
        assert!(
            (immediate - delayed).abs() < 0.1,
            "immediate={immediate}, delayed={delayed}"
        );
    }

    /// MAN-495: doctor observes every stage it does not keep applied (an
    /// all-off config runs them as observe) and reports the DC offset over
    /// the channel noise, so the impairment a `dc_block = "apply"` would
    /// remove is named instead of silently degrading the decode.
    #[test]
    fn doctor_reports_a_dc_offset_over_the_channel_noise() {
        let report = doctor(
            dc_offset_source((0.01, 0.007), 4.0),
            &PipelineConfig::default(),
            Duration::from_secs(3),
        )
        .unwrap();
        let cond = report
            .input_conditioning
            .as_ref()
            .expect("a receiver ran, so the report exists");
        assert_eq!(cond.impulse.mode, "observe", "{cond:?}");
        assert_eq!(cond.dc.mode, "observe", "{cond:?}");
        // 20*log10(|(0.01, 0.007)|) = -38.3 dBFS.
        let level = cond.dc.level_dbfs.expect("a DC level was measured");
        assert!((-45.0..=-30.0).contains(&level), "{cond:?}");
        let over = cond
            .dc
            .over_channel_noise_db
            .expect("the floor tap measured a reference");
        assert!(over >= 12.0, "{over} dB over the channel noise: {cond:?}");
        // Observe never writes: the DC offset is still in the audio and
        // nothing was repaired or corrected.
        assert_eq!(cond.dc.corrected_samples, 0, "{cond:?}");
        assert_eq!(cond.impulse.repaired_samples, 0, "{cond:?}");
    }

    /// MAN-495: doctor counts an injected impulse without repairing it
    /// (observe measures, never writes), and its decode tabulation equals
    /// an unconditioned run's over the same samples.
    #[test]
    fn doctor_counts_a_flagged_impulse_without_repairing_it() {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 6.0,
            ..manta_testkit::vectors::v1()
        };
        let mut samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        // One huge single-sample impulse, the MAN-203 fixture amplitude.
        samples[200_000] = Complex32::new(1e8, 0.0);
        let bounded = &samples[..(spec.fs * 5.0).round() as usize];
        let report = doctor(
            Box::new(RawIqSource {
                samples: samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            }),
            &PipelineConfig::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        let cond = report.input_conditioning.as_ref().expect("observed");
        assert_eq!(
            cond.impulse.flagged_samples, 1,
            "the injected impulse was not flagged exactly: {cond:?}"
        );
        assert_eq!(cond.impulse.repaired_samples, 0, "{cond:?}");
        assert_eq!(cond.impulse.paused_samples, 0, "{cond:?}");
        // Observe never writes: doctor's decode counts are exactly an
        // unconditioned run's over the same samples.
        let (promoted, chars) = unconditioned_counts(bounded, spec.fs, spec.center_freq_hz);
        assert_eq!(report.tracks_promoted, promoted, "{report:?}");
        assert_eq!(report.chars_decoded, chars, "{report:?}");
    }

    /// MAN-495: a stage the operator set to `apply` stays applied in
    /// doctor's run -- doctor only adds observation, it never takes a
    /// configured correction away.
    #[test]
    fn doctor_keeps_apply_stages_applied() {
        let cfg = PipelineConfig {
            conditioning: crate::ConditioningConfig {
                dc: manta_dsp::condition::StageMode::Apply,
                ..Default::default()
            },
            ..Default::default()
        };
        let report = doctor(
            dc_offset_source((0.01, 0.007), 4.0),
            &cfg,
            Duration::from_secs(3),
        )
        .unwrap();
        let cond = report.input_conditioning.as_ref().expect("observed");
        assert_eq!(cond.dc.mode, "apply", "{cond:?}");
        assert!(
            cond.dc.corrected_samples > 0,
            "the apply DC stage corrected nothing: {cond:?}"
        );
        // The off impulse stage was forced to observe, not left off.
        assert_eq!(cond.impulse.mode, "observe", "{cond:?}");
    }

    /// MAN-495: `doctor --json` carries `inputConditioning` right after
    /// `badSamples`, and a report with none (checks-only) keeps it absent.
    #[test]
    fn doctor_json_is_camel_case_and_carries_input_conditioning() {
        let mut report = no_promotion_report(1);
        report.bad_samples = 1;
        report.input_conditioning = Some(ConditioningReport {
            impulse: crate::ImpulseConditioning {
                mode: "observe".into(),
                ..Default::default()
            },
            dc: crate::DcConditioning {
                mode: "observe".into(),
                level_dbfs: Some(-40.1),
                over_channel_noise_db: Some(18.2),
                corrected_samples: 0,
            },
            spans: Vec::new(),
            spans_dropped: 0,
        });
        let s = serde_json::to_string(&report).unwrap();
        assert_eq!(
            crate::json_keys_in_order(&s)[..],
            [
                "sampleRateHz",
                "centerFreqHz",
                "durationS",
                "tracksPromoted",
                "detectorConfirmations",
                "trackMetaCount",
                "tracksClosed",
                "snrMin",
                "snrMedian",
                "snrMax",
                "charsDecoded",
                "distinctChars",
                "spotsConfirmed",
                "badSamples",
                "inputConditioning",
            ][..]
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["inputConditioning"]["dc"]["overChannelNoiseDb"], 18.2);
        report.input_conditioning = None;
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("inputConditioning"));
    }
    /// MAN-530: digital silence at 48 kHz from a source whose shared health
    /// counters grow WHILE doctor reads it, the way a live receiver's
    /// capture thread counts losses: `gaps_per_read` gap events and
    /// `packets_per_read` lost packets (HPSDR-style) land on every read, so
    /// a snapshot taken at doctor's entry would read 0 and only the final
    /// one matches the counter after the run.
    struct GrowingHealthSource {
        health: Option<Arc<manta_input::InputHealthCounters>>,
        gaps_per_read: u64,
        packets_per_read: u64,
    }

    impl IqSource for GrowingHealthSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if let Some(h) = &self.health {
                for _ in 0..self.gaps_per_read {
                    h.record_gap();
                }
                if self.packets_per_read > 0 {
                    h.record_dropped(self.packets_per_read);
                }
            }
            buf.fill(Complex32::new(0.0, 0.0));
            Ok(buf.len())
        }
        fn health_counters(&self) -> Option<Arc<manta_input::InputHealthCounters>> {
            self.health.clone()
        }
    }

    fn growing_health_report(
        health: Option<Arc<manta_input::InputHealthCounters>>,
        gaps_per_read: u64,
        packets_per_read: u64,
    ) -> DoctorReport {
        doctor(
            Box::new(GrowingHealthSource {
                health,
                gaps_per_read,
                packets_per_read,
            }),
            &PipelineConfig::default(),
            MIN_DURATION,
        )
        .unwrap()
    }

    /// MAN-530: a gap-only source (a SoapySDR overflow carries no count)
    /// reports this run's FINAL gap total, counted during the run --
    /// startup calibration included -- and the verdict stays what the
    /// existing rules give digital silence.
    #[test]
    fn doctor_reports_the_final_gap_count_of_a_source_whose_counters_grow() {
        let health = Arc::new(manta_input::InputHealthCounters::new());
        let report = growing_health_report(Some(health.clone()), 1, 0);
        assert!(health.gaps_detected() > 0, "the source counted gaps");
        assert_eq!(report.input_gaps_detected, health.gaps_detected());
        assert_eq!(report.queue_dropped_samples, 0);
        assert_eq!(report.verdict(), Verdict::NoSignal);
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["inputGapsDetected"], health.gaps_detected());
        assert!(v.get("queueDroppedSamples").is_none());
    }

    /// MAN-530: HPSDR-style packet loss (dropped packets plus a gap event)
    /// reaches the report as gaps only: dropped packets are a separate
    /// metric family, not capture-queue samples.
    #[test]
    fn doctor_reports_packet_loss_gaps_but_not_packets_as_queue_drops() {
        let health = Arc::new(manta_input::InputHealthCounters::new());
        let report = growing_health_report(Some(health.clone()), 1, 3);
        assert!(health.dropped_packets() > 0);
        assert_eq!(report.input_gaps_detected, health.gaps_detected());
        assert_eq!(report.queue_dropped_samples, health.queue_dropped_samples());
        assert_eq!(report.queue_dropped_samples, 0);
        assert_eq!(report.verdict(), Verdict::NoSignal);
    }

    /// MAN-530: a clean source and a source with no health counters at all
    /// (a file) report zero loss, and the JSON keeps its pre-MAN-530 shape.
    #[test]
    fn doctor_reports_no_loss_for_clean_and_counterless_sources() {
        for health in [
            Some(Arc::new(manta_input::InputHealthCounters::new())),
            None,
        ] {
            let report = growing_health_report(health, 0, 0);
            assert_eq!(report.queue_dropped_samples, 0);
            assert_eq!(report.input_gaps_detected, 0);
            assert_eq!(report.verdict(), Verdict::NoSignal);
            let s = serde_json::to_string(&report).unwrap();
            assert!(!s.contains("queueDroppedSamples"), "{s}");
            assert!(!s.contains("inputGapsDetected"), "{s}");
        }
    }

    /// MAN-530: `queueDroppedSamples` and `inputGapsDetected` are top-level
    /// camelCase members after `inputConditioning`, each omitted on its own
    /// when zero.
    #[test]
    fn doctor_json_carries_each_input_loss_member_only_when_nonzero() {
        let mut report = no_promotion_report(0);
        let keys = |r: &DoctorReport| {
            let s = serde_json::to_string(r).unwrap();
            crate::json_keys_in_order(&s)
                .into_iter()
                .filter(|k| k == "queueDroppedSamples" || k == "inputGapsDetected")
                .collect::<Vec<_>>()
        };
        assert!(keys(&report).is_empty());
        report.queue_dropped_samples = 15_808;
        assert_eq!(keys(&report), ["queueDroppedSamples"]);
        report.input_gaps_detected = 1;
        assert_eq!(keys(&report), ["queueDroppedSamples", "inputGapsDetected"]);
        report.queue_dropped_samples = 0;
        assert_eq!(keys(&report), ["inputGapsDetected"]);
        report.queue_dropped_samples = 15_808;
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["queueDroppedSamples"], 15_808);
        assert_eq!(v["inputGapsDetected"], 1);
        let all = crate::json_keys_in_order(&serde_json::to_string(&report).unwrap());
        assert_eq!(
            &all[all.len() - 2..],
            ["queueDroppedSamples", "inputGapsDetected"]
        );
    }
}
