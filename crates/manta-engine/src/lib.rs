//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod check;
pub mod config_file;
pub mod listen;
pub use listen::{
    listen, listen_with_observers, ListenObservers, OperatorLists, OperatorListsUpdate,
    RttyObserver, SuppressionObserver,
};
pub mod latency;
pub mod live;
pub mod multi;
pub use latency::{DecodeLatencyObserver, DecodeLatencySnapshot, DECODE_LATENCY_BUCKETS_SECONDS};
pub use live::{
    BandSample, LiveRecord, LiveStateSink, TrackSnapshot, LIVE_MAX_CANDIDATES, LIVE_TEXT_CHARS,
};
pub mod lifecycle;
pub use lifecycle::{TrackLifecycleObserver, TrackLifecycleSnapshot};
pub use multi::{
    common_rate_hz, listen_inputs, open_inputs, InputEnd, InputObservers, InputOpener, InputSpec,
    OpenedInput, OpenedInputs, SessionClock, SharedObservers, SourceClock, TrackIdSpace,
    MAX_INPUTS,
};
pub mod quality;
pub use quality::{DecodeQualityObserver, DecodeQualitySnapshot, RAIL_RATIO_BUCKETS};
pub mod calibrate;
pub use calibrate::{calibrate, CalibrateOptions, CalibrationReport};
pub mod doctor;
pub use doctor::{doctor, DoctorReport, Verdict, MAX_DURATION, MIN_DURATION};
pub mod soak;
pub use manta_spot::reference::ReferenceLists;
pub use manta_spot::{Blocklist, NotchList, Spot, SpotType};
pub use soak::{soak, soak_passed, SoakReport};
pub mod soak_metrics;
pub use soak_metrics::{
    soak_metrics_passed, soak_with_metrics, SoakCloseCounts, SoakMetricsReport, SoakMetricsSample,
};

mod digital_windows;
#[cfg(test)]
mod static_crash_tests;
mod track;
pub use manta_dsp::crash::CRASH_MIN_LIVE_CHANNELS;
/// MAN-234: the input sample guard's limit (SPEC §1.3 step 0), for the
/// CLI's wording.
pub use manta_dsp::sample_guard::MAX_SAMPLE_MAGNITUDE;
pub use track::{crash_live_channels, ChannelLevel, CrashFilter, DetectorConfig};

use anyhow::{bail, Context, Result};
use manta_decode::decoder::{events_to_text, DecodeConfig};
use manta_decode::events::DecoderEvent;
use manta_decode::timing::TimingEstimates;
use manta_input::{IqSource, WavIqSource};
use num_complex::Complex32;
use std::path::Path;
use std::sync::Arc;

/// Applies the calibration factor to every event variant that carries a
/// `freq_hz` (`TrackMeta`, `TrackPromoted`), leaving every other variant
/// untouched. Used to calibrate the public-facing event stream
/// (`DecodeReport::events`, `listen()`'s `on_event` callback) -- both
/// consumed directly by `decode --json`/`listen --json` -- WITHOUT
/// touching the copy fed to `manta_spot::Validator::ingest`, which already
/// applies its own calibration internally; applying it here too would
/// double-correct the validator's spot output (MAN-29 review round 3).
/// `TrackPromoted` calibration added in round 5 of the doctor review:
/// otherwise a promoted track's first-reported frequency is raw while
/// every later `TrackMeta`/spot for the same track is calibrated.
pub(crate) fn calibrate_freq_events(ev: &DecoderEvent, factor: f64) -> DecoderEvent {
    match ev {
        DecoderEvent::TrackMeta {
            track_id,
            sample_ts,
            snr_2500_db,
            freq_hz,
        } => DecoderEvent::TrackMeta {
            track_id: *track_id,
            sample_ts: *sample_ts,
            snr_2500_db: *snr_2500_db,
            freq_hz: freq_hz * factor,
        },
        DecoderEvent::TrackPromoted {
            track_id,
            sample_ts,
            freq_hz,
        } => DecoderEvent::TrackPromoted {
            track_id: *track_id,
            sample_ts: *sample_ts,
            freq_hz: freq_hz * factor,
        },
        other => other.clone(),
    }
}

/// The last `TimingUpdate` in `track_events` (one track's events): the
/// sender timing `DecodeReport::timing` reports (MAN-315).
fn last_timing(track_events: &[DecoderEvent]) -> Option<TimingEstimates> {
    track_events.iter().rev().find_map(|e| match e {
        DecoderEvent::TimingUpdate { timing, .. } => Some(timing.clone()),
        _ => None,
    })
}

/// `decode_samples`'s single-track selection: the lowest track_id among
/// events that represent real decoder output. `TrackPromoted` is a
/// detector-internal diagnostic signal (added for `manta_engine::doctor()`'s
/// NoSignal check, see docs/DECISIONS/2026-09-09-doctor-track-promoted-
/// event.md) with no decoder output of its own -- excluded here so an
/// early, low-track-id candidate that was promoted and then merged/
/// evicted/reached EOF before producing any real decoder event never gets
/// selected over a later track that actually decoded something (round-5
/// review finding). `TrackClosed` is bookkeeping, not decoder output,
/// and is excluded too (MAN-174): a merge survivor that never decoded
/// still gets its own `TrackClosed`, and must not become the reported
/// track on that alone. `None` means every event in `events` was a
/// `TrackPromoted` or `TrackClosed` (or `events` was empty, already
/// handled by the caller before this is reached).
fn primary_track_id(events: &[DecoderEvent]) -> Option<u32> {
    events
        .iter()
        .filter(|e| {
            !matches!(
                e,
                DecoderEvent::TrackPromoted { .. } | DecoderEvent::TrackClosed { .. }
            )
        })
        .map(track::event_track_id)
        .min()
}

/// M0 pipeline tunables. SPEC §5.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Classical decoder tunables. SPEC §5.
    pub decode: DecodeConfig,
    /// Real multi-track detector tunables. SPEC §9 `[detector]` table.
    pub detector: track::DetectorConfig,
    /// Per-source frequency-calibration correction, in ppm (config key
    /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4; 0.0 = no
    /// correction). Applied to both the top-level `DecodeReport::freq_hz`
    /// and every emitted spot's `freq_hz`, so the two never disagree.
    /// Corrects a drifted source clock/LO -- distinct from
    /// `manta-spot`'s ~10 Hz decode-accuracy figure (ARCHITECTURE §6 step
    /// 5), which is decode precision (MAN-29).
    pub freq_correction_ppm: f64,
    /// Operator Watch List (ARCHITECTURE §6, MAN-28): callsigns here
    /// bypass grammar/cty validation and the repetition gate entirely in
    /// the production validator, matching CW Skimmer's Watch List.
    pub allowlist: Vec<String>,
    /// Operator's bad-callsign blocklist (MAN-31). Empty by default -- no
    /// suppression until the operator supplies one.
    pub blocklist: Blocklist,
    /// Operator's notched-frequency list (MAN-31). Empty by default -- no
    /// suppression until the operator supplies one.
    pub notch: NotchList,
    /// Operator-supplied cty.dat, parsed once. None uses the bundled table.
    pub cty: Option<Arc<manta_spot::cty::Table>>,
    /// Operator-supplied MASTER.SCP, parsed once. None uses the bundled set.
    pub scp: Option<Arc<manta_spot::scp::Set>>,
    /// Operator-supplied callsign-pattern table. None uses the bundled list.
    pub patterns: Option<Arc<manta_spot::pattern::Table>>,
    /// Repetition thresholds by callsign-pattern rarity.
    pub validation_level: manta_spot::ValidationLevel,
}

impl PipelineConfig {
    /// The resolved country table, shared with the daemon's geography lookup.
    pub fn cty_table(&self) -> Arc<manta_spot::cty::Table> {
        self.cty
            .clone()
            .unwrap_or_else(|| Arc::new(manta_spot::cty::Table::bundled()))
    }
    /// The resolved known-callsign set.
    pub fn scp_set(&self) -> Arc<manta_spot::scp::Set> {
        self.scp
            .clone()
            .unwrap_or_else(|| Arc::new(manta_spot::scp::Set::bundled()))
    }
    /// The resolved callsign-pattern table.
    pub fn pattern_table(&self) -> Arc<manta_spot::pattern::Table> {
        self.patterns
            .clone()
            .unwrap_or_else(|| Arc::new(manta_spot::pattern::Table::bundled()))
    }
    /// Builds the validator used by every production decode path.
    pub fn validator(&self, fs: f64) -> Result<manta_spot::Validator> {
        // Keep the checked ppm constructor so callers cannot supply an
        // unchecked raw calibration factor (MAN-29).
        let mut validator =
            manta_spot::Validator::from_tables(fs, self.cty_table(), Some(self.scp_set()))
                .with_freq_correction_ppm(self.freq_correction_ppm)
                .map_err(|e| anyhow::anyhow!(e))?
                .with_blocklist(self.blocklist.clone())
                .with_notch(self.notch.clone())
                .with_patterns(self.pattern_table())
                .with_validation_level(self.validation_level);
        for call in &self.allowlist {
            validator.allowlist(call);
        }
        Ok(validator)
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            decode: DecodeConfig::default(),
            detector: track::DetectorConfig::default(),
            freq_correction_ppm: 0.0,
            allowlist: Vec::new(),
            blocklist: Blocklist::default(),
            notch: NotchList::default(),
            cty: None,
            scp: None,
            patterns: None,
            validation_level: manta_spot::ValidationLevel::default(),
        }
    }
}

/// Result of decoding one signal from an IQ scene. SPEC §5.
#[derive(Debug, serde::Serialize)]
pub struct DecodeReport {
    /// Absolute spot frequency (center + estimated offset), full precision.
    /// SPEC §1.4: full Hz precision belongs to the JSON surface.
    pub freq_hz: f64,
    /// Most recently reported speed, if any.
    pub wpm: Option<f32>,
    /// Assembled plain text (see events_to_text).
    pub text: String,
    /// The full decoder event stream, for JSON output.
    pub events: Vec<DecoderEvent>,
    /// Validated spots (`manta-spot::Validator`, ARCHITECTURE §6), run
    /// over the full multi-track event stream above.
    pub spots: Vec<Spot>,
    /// The primary track's last `TimingUpdate` (its learned dot, dash and
    /// gap timing), or `None` when it emitted none (MAN-315).
    pub timing: Option<TimingEstimates>,
    /// MAN-314: every RTTY signal the detector marked (SPEC §2.6); empty
    /// for input without RTTY or with `rtty_filter` off.
    pub rtty: Vec<RttySignalReport>,
    /// MAN-234: input samples replaced with silence (SPEC §1.3 step 0).
    /// Omitted from JSON when 0, so a clean decode's `--json` is unchanged.
    #[serde(skip_serializing_if = "is_zero")]
    pub bad_samples: u64,
    /// Input sample rate, for turning `rtty` sample timestamps into
    /// seconds; not serialized.
    #[serde(skip)]
    pub sample_rate_hz: f64,
}

/// `skip_serializing_if` predicate for the MAN-234 `bad_samples` report
/// fields: a clean run's JSON keeps its pre-MAN-234 shape.
pub(crate) fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// MAN-234: the suffix `decode_samples` adds to its "no signal found"
/// errors when the channelizer replaced any input samples, so an input
/// that was all bad samples says so instead of only "no signal".
fn bad_samples_suffix(n: u64) -> String {
    if n == 0 {
        return String::new();
    }
    let (noun, verb) = if n == 1 {
        ("sample", "was")
    } else {
        ("samples", "were")
    };
    format!(
        "; {n} bad input {noun} (NaN, infinite, or larger than {:e} in magnitude) {verb} \
         replaced with silence",
        MAX_SAMPLE_MAGNITUDE
    )
}

/// MAN-314: one marked RTTY signal (SPEC §2.6), frequencies calibrated
/// like `TrackMeta::freq_hz`. `marked_sample_ts`/`cleared_sample_ts` are
/// stream sample timestamps; `cleared_sample_ts` is `None` while marked.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RttySignalReport {
    /// Upper tone.
    pub mark_hz: f64,
    /// Lower tone.
    pub space_hz: f64,
    /// Nearest standard shift (170/200/425/450/850 Hz).
    pub shift_hz: f64,
    /// Measured `mark_hz - space_hz`, uncalibrated.
    pub measured_shift_hz: f64,
    pub marked_sample_ts: u64,
    pub cleared_sample_ts: Option<u64>,
    /// Detector id, stable while marked; not serialized.
    #[serde(skip)]
    pub id: u32,
}

impl RttySignalReport {
    /// `entry` with its frequencies multiplied by `calibration_factor`.
    pub(crate) fn from_entry(entry: &track::RttyLogEntry, calibration_factor: f64) -> Self {
        RttySignalReport {
            mark_hz: entry.mark_hz * calibration_factor,
            space_hz: entry.space_hz * calibration_factor,
            shift_hz: entry.shift_hz,
            measured_shift_hz: entry.measured_shift_hz,
            marked_sample_ts: entry.marked_sample_ts,
            cleared_sample_ts: entry.cleared_sample_ts,
            id: entry.id,
        }
    }
}

/// M0 pipeline: estimate frequency, extract one channel, decode. SPEC
/// §1.3–§1.4, §3–§5.
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    decode_source(
        &mut SliceSource {
            iq,
            fs,
            center_freq_hz,
        },
        cfg,
    )
}

/// An in-memory `IqSource` over a sample slice, for `decode_samples`.
struct SliceSource<'a> {
    iq: &'a [Complex32],
    fs: f64,
    center_freq_hz: f64,
}

impl IqSource for SliceSource<'_> {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = buf.len().min(self.iq.len());
        buf[..n].copy_from_slice(&self.iq[..n]);
        self.iq = &self.iq[n..];
        Ok(n)
    }
}

/// One gap-free stretch of an offline decode (MAN-309): its own channelizer
/// and track manager, as `listen`'s `Segment`, whose sample clock starts at
/// `base` on the source's timeline.
struct OfflineSegment {
    ch: manta_dsp::channelizer::Channelizer,
    tm: track::TrackManager,
    hop: u64,
    pad_hops: u64,
    pad_samples: usize,
    base: u64,
}

/// The source's passband, trusted band and analytic guard, read once and
/// applied to every offline segment (MAN-309), as `listen` applies them.
#[derive(Clone, Copy)]
struct SourceBands {
    fs: f64,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    trusted_band_hz: Option<(f64, f64)>,
    guard_hz: f64,
    analytic: bool,
}

impl SourceBands {
    fn of(src: &dyn IqSource, cfg: &PipelineConfig) -> Self {
        SourceBands {
            fs: src.sample_rate(),
            center_freq_hz: src.center_freq_hz(),
            rf_passband_hz: src.rf_passband_hz(),
            trusted_band_hz: src.trusted_band_hz(),
            guard_hz: cfg.detector.guard_hz.max(src.analytic_guard_hz()),
            analytic: src.analytic_guard_hz() > 0.0,
        }
    }

    /// A fresh segment starting at `base`, numbering tracks from `next_id`.
    /// The band setters are no-ops for a plain complex source (full
    /// Nyquist, no trusted band, zero guard), so a WAV or slice decode is
    /// unchanged while `DetectorConfig::guard_hz` is 0 (the config loader
    /// rejects `[detector] guard_hz`, so only code can raise it).
    fn segment(&self, cfg: &PipelineConfig, base: u64, next_id: u32) -> Result<OfflineSegment> {
        let ch = manta_dsp::channelizer::Channelizer::new(self.fs, self.center_freq_hz)
            .map_err(|e| anyhow::anyhow!(e))
            .context("channelizer")?;
        let hop = ch.hop() as u64;
        debug_assert!(
            (self.fs / hop as f64 - manta_decode::FO_HZ).abs() < 0.01,
            "channelizer hop rate {} Hz diverges from manta_decode::FO_HZ {}",
            self.fs / hop as f64,
            manta_decode::FO_HZ
        );
        let pad_samples = ch.filter_len();
        let pad_hops = (pad_samples as u64).div_ceil(hop);
        let mut tm = track::TrackManager::new(
            ch.n_channels(),
            self.fs,
            self.center_freq_hz,
            cfg.detector,
            cfg.decode.clone(),
        );
        tm.resume_track_ids_from(next_id);
        tm.skip_preroll_hops(pad_hops);
        tm.set_rf_passband_hz(self.rf_passband_hz.0, self.rf_passband_hz.1);
        if let Some((lo, hi)) = self.trusted_band_hz {
            tm.set_trusted_band_hz(lo, hi);
        }
        if self.guard_hz > 0.0 {
            tm.set_guard_hz(self.guard_hz);
        }
        tm.set_mirror_image_guard(self.analytic);
        // MAN-314: `DecodeReport::rtty` lists every marked signal, so the
        // streaming cap on the RTTY log does not apply to a file decode.
        tm.keep_full_rtty_history();
        Ok(OfflineSegment {
            ch,
            tm,
            hop,
            pad_hops,
            pad_samples,
            base,
        })
    }
}

/// Appends `tm`'s marked RTTY signals to `rtty` (MAN-309): each segment's
/// detector numbers its signals from 1, so a later segment's ids continue
/// after the largest already reported. One segment's ids are unchanged.
/// At a discontinuity, active marks clear where the segment's samples end.
fn append_rtty(
    rtty: &mut Vec<RttySignalReport>,
    tm: &track::TrackManager,
    calibration_factor: f64,
    gap_end: Option<u64>,
) {
    let base = rtty.iter().map(|r| r.id).max().unwrap_or(0);
    rtty.extend(tm.rtty_signals().iter().map(|e| {
        let mut row = RttySignalReport::from_entry(e, calibration_factor);
        row.id += base;
        row.cleared_sample_ts = row.cleared_sample_ts.or(gap_end);
        row
    }));
}

/// `decode_samples` over a source (MAN-119): reads `src` to EOF in
/// `CHUNK_SAMPLES` chunks of the zero-padded stream and holds no copy of the
/// input, so memory does not grow with the recording. Its output is identical
/// to `decode_samples` over the same samples, whatever sizes `src`'s reads
/// return. SPEC §1.3–§1.4, §3–§5.
///
/// MAN-309: a discontinuity the source reports (`take_discontinuity`, as a
/// replayed IQ recording's gaps) closes the current segment as `listen`
/// does -- `finish_for_discontinuity`, not an end of signal -- and starts a
/// fresh channelizer and track manager whose clock is advanced past the
/// missing samples and whose track ids continue. Missing samples are never
/// synthesized; the validator spans every segment. A gap-free source
/// decodes exactly as before.
pub fn decode_source(src: &mut dyn IqSource, cfg: &PipelineConfig) -> Result<DecodeReport> {
    // Validated up front so a bad config value (NaN, infinite, or ppm so
    // negative it flips the factor negative/zero) fails fast rather than
    // after the channelizer/decoder work below (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let bands = SourceBands::of(src, cfg);
    let fs = bands.fs;
    let center_freq_hz = bands.center_freq_hz;
    let mut seg = bands.segment(cfg, 0, 1)?;
    // Feed the channelizer/track-manager in bounded chunks rather than one
    // `ch.process(&whole_input)` + one `tm.process_hops(...)` call over the
    // whole file. `Channelizer::process` is chunk-size-invariant (proved by
    // `channelizer_chunking_determinism.rs`/`chunking_determinism.rs`), so
    // this doesn't change any hop's numeric output -- but `TrackManager`'s
    // SPEC §2.4 GC/silent-timer reset (`Lifecycle::note_char_decoded`) only
    // runs *after* `process_hops`'s `drain_pool()` call, once per call, for
    // whatever `CharDecoded` events that call's batch produced. Every hop in
    // between is driven by `step_hop` with `char_emitted = false` (the pool
    // hasn't run yet -- see `step_hop`'s doc), so a single whole-file
    // `process_hops` call lets `silent_count` climb unchecked for the
    // *entire* file before the one and only GC-timer reset ever happens;
    // any file longer than `gc_hops` (30 s default) force-closes its own
    // track partway through as `CloseReason::Silent`, even though the
    // signal never stopped -- discovered via a 120 s golden-vector decode
    // silently truncating to ~30 s of text. Chunking keeps `process_hops`
    // (and so `note_char_decoded`) running frequently enough that a
    // continuously-decoding track's GC timer never falsely expires. The
    // chunks are those of each segment's zero-padded stream, whatever sizes
    // `src`'s reads return (MAN-119).
    const CHUNK_SAMPLES: usize = 4096;
    let mut events = Vec::new();
    let mut rtty: Vec<RttySignalReport> = Vec::new();
    let mut bad_samples = 0u64;
    let mut any_hops = false;
    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    // Samples read after a gap, held for the next segment while the
    // previous one processes its partial chunk (MAN-309).
    let mut carry: Vec<Complex32> = Vec::new();
    let mut carry_at = 0usize;
    let (mut pad_left, mut nonzero, mut eof) = (seg.pad_samples, false, false);
    let mut consumed = 0u64;
    loop {
        // The segment's zero pre-roll first, then held samples, then the
        // source's.
        let mut filled = pad_left.min(CHUNK_SAMPLES);
        chunk[..filled].fill(Complex32::new(0.0, 0.0));
        pad_left -= filled;
        let held = (carry.len() - carry_at).min(CHUNK_SAMPLES - filled);
        chunk[filled..filled + held].copy_from_slice(&carry[carry_at..carry_at + held]);
        carry_at += held;
        filled += held;
        consumed += held as u64;
        if carry_at == carry.len() {
            carry.clear();
            carry_at = 0;
        }
        let mut gap = None;
        while !eof && filled < CHUNK_SAMPLES && carry.is_empty() {
            let n = src.read(&mut chunk[filled..])?;
            if n == 0 {
                eof = true;
                break;
            }
            nonzero |= chunk[filled..filled + n]
                .iter()
                .any(|s| s.re != 0.0 || s.im != 0.0);
            if let Some(missing) = src.take_discontinuity() {
                // These samples follow the outage: they open the next
                // segment, after this one's partial chunk.
                carry.extend_from_slice(&chunk[filled..filled + n]);
                gap = Some(missing);
                break;
            }
            filled += n;
            consumed += n as u64;
        }
        if filled > 0 {
            let hops = seg.ch.process(&chunk[..filled]);
            any_hops |= !hops.is_empty();
            let (base, pad_hops, hop) = (seg.base, seg.pad_hops, seg.hop);
            events.extend(
                seg.tm
                    .process_hops(&hops, |m| base + m.saturating_sub(pad_hops) * hop),
            );
        }
        let Some(missing) = gap else {
            if eof || filled == 0 {
                break;
            }
            continue;
        };
        // Not `finish()`: an outage is not an end of signal (MAN-73).
        let end = seg
            .base
            .checked_add(consumed)
            .context("input discontinuity overflows the sample clock")?;
        append_rtty(&mut rtty, &seg.tm, calibration_factor, Some(end));
        events.extend(seg.tm.finish_for_discontinuity());
        bad_samples += seg.ch.bad_samples();
        let base = end
            .checked_add(missing)
            .context("input discontinuity overflows the sample clock")?;
        seg = bands.segment(cfg, base, seg.tm.next_track_id())?;
        pad_left = seg.pad_samples;
        consumed = 0;
    }
    // MAN-258: a stream that ended during an outage is not an end of signal.
    let terminal_gap = src.take_discontinuity().is_some();
    if !nonzero {
        bail!("input is digital silence");
    }
    bad_samples += seg.ch.bad_samples();
    if !any_hops {
        bail!(
            "no signal found (input shorter than one filter length or empty){}",
            bad_samples_suffix(bad_samples)
        );
    }
    let end = seg
        .base
        .checked_add(consumed)
        .context("input sample clock overflows")?;
    append_rtty(
        &mut rtty,
        &seg.tm,
        calibration_factor,
        terminal_gap.then_some(end),
    );
    events.extend(if terminal_gap {
        seg.tm.finish_for_discontinuity()
    } else {
        seg.tm.finish()
    });

    // MAN-314: a marked RTTY signal is a finding on its own, so input with
    // RTTY but no CW decoder output still returns a report (empty CW
    // fields, `rtty` filled) instead of "no signal found". MAN-313: hops
    // were produced, so the input was long enough; with `non_cw_filter` on
    // an input of only speech, data or swept carriers lands here.
    if events.is_empty() && rtty.is_empty() {
        bail!(
            "no signal found (no keyed CW signal in the input){}",
            bad_samples_suffix(bad_samples)
        );
    }
    let min_track_id = primary_track_id(&events);
    if min_track_id.is_none() && rtty.is_empty() {
        bail!(
            "no signal found (only detector promotion events, no decoder output){}",
            bad_samples_suffix(bad_samples)
        );
    }
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| Some(track::event_track_id(e)) == min_track_id)
        .cloned()
        .collect();
    let freq_hz = this_track
        .iter()
        .rev()
        .find_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .unwrap_or(center_freq_hz)
        * calibration_factor;
    let wpm = this_track.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&this_track);
    let timing = last_timing(&this_track);
    let mut validator = cfg.validator(fs)?;
    let mut spots = Vec::new();
    for ev in &events {
        spots.extend(validator.ingest(ev));
    }
    // Mutated in place, after the validator above has already ingested the
    // raw values (validator.ingest() applied its own correction to its
    // internal spot output, so this must not run before that loop) --
    // collecting into a second `Vec` here would clone every event while
    // Rust keeps the original alive until this function returns, doubling
    // peak memory on a long/dense offline decode for no reason (MAN-29
    // review round 4).
    for ev in events.iter_mut() {
        match ev {
            DecoderEvent::TrackMeta { freq_hz, .. }
            | DecoderEvent::TrackPromoted { freq_hz, .. } => {
                *freq_hz *= calibration_factor;
            }
            _ => {}
        }
    }
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
        spots,
        timing,
        rtty,
        bad_samples,
        sample_rate_hz: fs,
    })
}

/// decode_samples, sourced from a WAV file via manta-input. ARCHITECTURE
/// §3; SPEC §3–§5. `center_freq_hz` overrides the sidecar's centre
/// frequency, and the sidecar is then not read (MAN-131, `decode
/// --center-freq-hz`); `None` keeps the sidecar's value, `0.0` without one.
pub fn decode_wav(
    path: &Path,
    center_freq_hz: Option<f64>,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    let mut src = match center_freq_hz {
        Some(hz) => WavIqSource::open_with_center_freq_hz(path, hz)?,
        None => WavIqSource::open(path)?,
    };
    decode_source(&mut src, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The QQ9ZZZ fixture `cli.rs`'s allowlist test uses: V1, 30 s, an
    /// unallocated-prefix call.
    fn qq9zzz_spec() -> manta_testkit::vectors::VectorSpec {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 30.0;
        spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
        spec
    }

    fn cty_with_qq9() -> Arc<manta_spot::cty::Table> {
        Arc::new(manta_spot::cty::Table::parse(&format!(
            "{}Test DXpedition:  14:  27:  EU:  50.0:  -5.0:  0.0:  QQ9:\n    QQ9;\n",
            manta_spot::CTY_DAT
        )))
    }

    #[test]
    fn decode_samples_spots_a_call_only_an_override_cty_allocates() {
        let spec = qq9zzz_spec();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let calls = |cfg: &PipelineConfig| -> Vec<String> {
            decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, cfg)
                .unwrap()
                .spots
                .into_iter()
                .map(|s| s.callsign)
                .collect()
        };
        assert!(!calls(&PipelineConfig::default()).contains(&"QQ9ZZZ".to_string()));
        let cfg = PipelineConfig {
            cty: Some(cty_with_qq9()),
            patterns: Some(Arc::new(
                manta_spot::pattern::Table::parse("+QQ9@@@\n").unwrap(),
            )),
            ..Default::default()
        };
        assert!(
            calls(&cfg).contains(&"QQ9ZZZ".to_string()),
            "override must let QQ9ZZZ spot"
        );
    }

    #[test]
    fn default_pipeline_config_uses_the_built_in_tables() {
        let cfg = PipelineConfig::default();
        assert!(cfg.cty_table().is_allocated("W1AW"));
        assert!(!cfg.cty_table().is_allocated("QQ9ZZZ"));
        assert_eq!(cfg.scp_set().len(), manta_spot::scp::Set::bundled().len());
    }

    #[test]
    fn an_override_table_is_shared_not_copied() {
        let table = cty_with_qq9();
        let cfg = PipelineConfig {
            cty: Some(table.clone()),
            ..Default::default()
        };
        assert!(Arc::ptr_eq(&cfg.cty_table(), &table));
    }

    #[test]
    fn pipeline_config_debug_stays_short_with_tables_loaded() {
        let cfg = PipelineConfig {
            cty: Some(Arc::new(manta_spot::cty::Table::bundled())),
            scp: Some(Arc::new(manta_spot::scp::Set::bundled())),
            ..Default::default()
        };
        // Loaded tables add a size summary, not ~50k callsigns.
        let baseline = format!("{:?}", PipelineConfig::default()).len();
        assert!(format!("{cfg:?}").len() < baseline + 200);
    }

    /// Regression (round-5 review, P1): an early low-track-id candidate
    /// that only ever produced a `TrackPromoted` (promoted, then merged/
    /// evicted/EOF'd before any real decoder output) must never be
    /// selected over a later track that actually decoded something.
    #[test]
    fn primary_track_id_skips_a_promotion_only_track() {
        let events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 1,
                sample_ts: 0,
                freq_hz: 14_000_100.0,
            },
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 10,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::TrackMeta {
                track_id: 2,
                sample_ts: 15,
                snr_2500_db: 20.0,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 2,
                sample_ts: 20,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
                alternatives: Vec::new(),
            },
        ];
        assert_eq!(primary_track_id(&events), Some(2));
    }

    /// MAN-174: a merge survivor that never decoded still gets its own
    /// `TrackClosed`. That bookkeeping event must not make it the
    /// reported track over a higher-id track with real decoder output.
    #[test]
    fn primary_track_id_ignores_track_closed() {
        let events = vec![
            DecoderEvent::TrackClosed {
                track_id: 2,
                closure: manta_decode::events::ClosureKind::SignalEnded,
            },
            DecoderEvent::word_boundary(5, 10),
        ];
        assert_eq!(primary_track_id(&events), Some(5));
    }

    /// MAN-315: `DecodeReport::timing` is the primary track's last
    /// `TimingUpdate`, never another track's.
    #[test]
    fn decode_report_timing_is_primary_tracks_last() {
        use manta_decode::timing::ClassEstimate;
        let update = |track_id, sample_ts, dash: f32| DecoderEvent::TimingUpdate {
            track_id,
            sample_ts,
            timing: TimingEstimates {
                dit_ms: 48.0,
                dot: None,
                dash: Some(ClassEstimate {
                    mean_dits: dash,
                    var_dits2: 0.0,
                    n: 1,
                }),
                element_gap: None,
                char_gap: None,
                word_gap: None,
            },
        };
        let events = vec![
            update(2, 10, 3.0),
            update(3, 11, 2.6),
            update(2, 20, 4.0),
            update(3, 21, 2.7),
        ];
        let primary = primary_track_id(&events).unwrap();
        assert_eq!(primary, 2);
        let this_track: Vec<DecoderEvent> = events
            .iter()
            .filter(|e| track::event_track_id(e) == primary)
            .cloned()
            .collect();
        let t = last_timing(&this_track).unwrap();
        assert_eq!(t.dash.unwrap().mean_dits, 4.0);
        assert_eq!(last_timing(&[]), None);
    }

    #[test]
    fn primary_track_id_is_none_when_only_promotions_occurred() {
        let events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 1,
                sample_ts: 0,
                freq_hz: 14_000_100.0,
            },
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 5,
                freq_hz: 14_012_340.0,
            },
        ];
        assert_eq!(primary_track_id(&events), None);
    }

    /// MAN-31: `decode_samples` is one of the two production call sites
    /// that must apply an operator-supplied suppression list -- proves the
    /// `PipelineConfig` fields actually reach the `Validator`, not just the
    /// crate-level builders in isolation (those are already covered by
    /// `manta-spot`'s own golden_v16_v17 tests).
    #[test]
    fn decode_samples_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let report = decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert!(
            report.spots.is_empty(),
            "blocklisted callsign must never be spotted, got {:?}",
            report.spots
        );
    }

    /// MAN-234: V1 cut to 12 s, rendered once per test.
    fn v1_12s() -> (manta_testkit::vectors::VectorSpec, Vec<Complex32>) {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 12.0,
            ..manta_testkit::vectors::v1()
        };
        let samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        (spec, samples)
    }

    #[test]
    fn decode_samples_survives_a_bad_sample_mid_track() {
        // MAN-234 scenario 1: before the channelizer guard this panicked
        // at `track.rs`'s `select_channel` `partial_cmp(..).unwrap()`.
        let (spec, clean) = v1_12s();
        let cfg = PipelineConfig::default();
        let want = decode_samples(&clean, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert_eq!(want.bad_samples, 0);
        let mut corrupt = clean.clone();
        corrupt[576_000].re = f32::NAN; // t = 6 s
        let got = decode_samples(&corrupt, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert_eq!(got.bad_samples, 1);
        assert_eq!(got.text, want.text);
    }

    #[test]
    fn decode_samples_survives_a_finite_overflow_sample_at_start() {
        // MAN-234 D2: a finite `f32::MAX` latched the floor/gate EMAs at
        // NaN and the decode failed "no signal found".
        let (spec, clean) = v1_12s();
        let cfg = PipelineConfig::default();
        let want = decode_samples(&clean, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        let mut corrupt = clean.clone();
        corrupt[96].re = f32::MAX; // t = 1 ms
        let got = decode_samples(&corrupt, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert_eq!(got.bad_samples, 1);
        assert_eq!(got.text, want.text);
    }

    #[test]
    fn clean_decode_report_json_has_no_bad_samples_key() {
        let (spec, clean) = v1_12s();
        let mut report = decode_samples(
            &clean,
            spec.fs,
            spec.center_freq_hz,
            &PipelineConfig::default(),
        )
        .unwrap();
        let v = serde_json::to_value(&report).unwrap();
        assert!(v.get("bad_samples").is_none(), "clean JSON: {v}");
        report.bad_samples = 3;
        let s = serde_json::to_string(&report).unwrap();
        assert!(s.contains("\"bad_samples\":3"), "{s}");
    }

    #[test]
    fn no_signal_error_names_replaced_samples() {
        let cfg = PipelineConfig::default();
        let all_nan = vec![Complex32::new(f32::NAN, f32::NAN); 96_000];
        let err = decode_samples(&all_nan, 96_000.0, 14_000_000.0, &cfg)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("no signal found"), "{err}");
        assert!(
            err.contains(
                "96000 bad input samples (NaN, infinite, or larger than 1e17 in magnitude) \
                 were replaced with silence"
            ),
            "{err}"
        );
        let silence = vec![Complex32::new(0.0, 0.0); 96_000];
        let err = decode_samples(&silence, 96_000.0, 14_000_000.0, &cfg)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "input is digital silence");
        assert_eq!(
            bad_samples_suffix(1),
            "; 1 bad input sample (NaN, infinite, or larger than 1e17 in magnitude) was \
             replaced with silence"
        );
        assert_eq!(bad_samples_suffix(0), "");
    }

    #[test]
    fn decode_samples_suppresses_a_notched_frequency() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let signal_freq_hz = spec.center_freq_hz + spec.signals[0].offset_hz;
        let cfg = PipelineConfig {
            notch: manta_spot::NotchList::parse(&format!(
                "{}-{}\n",
                signal_freq_hz - 50.0,
                signal_freq_hz + 50.0
            )),
            ..Default::default()
        };
        let report = decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap();
        assert!(
            report.spots.is_empty(),
            "signal inside a notched range must never be spotted, got {:?}",
            report.spots
        );
    }
}
