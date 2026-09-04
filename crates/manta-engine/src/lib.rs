//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod listen;
pub use listen::listen;
pub mod soak;
pub use manta_spot::{Blocklist, NotchList, Spot, SpotType};
pub use soak::{soak, soak_passed, SoakReport};
pub mod soak_metrics;
pub use soak_metrics::{
    soak_metrics_passed, soak_with_metrics, SoakCloseCounts, SoakMetricsReport, SoakMetricsSample,
};

mod track;
pub use track::DetectorConfig;

use anyhow::{bail, Context, Result};
use manta_decode::decoder::{events_to_text, DecodeConfig};
use manta_decode::events::DecoderEvent;
use manta_input::{read_all, IqSource, WavIqSource};
use num_complex::Complex32;
use std::collections::BTreeMap;
use std::path::Path;

/// Applies the calibration factor to a `TrackMeta` event's `freq_hz`,
/// leaving every other event variant untouched. Used to calibrate the
/// public-facing event stream (`DecodeReport::events`, `listen()`'s
/// `on_event` callback) -- both consumed directly by `decode --json`/
/// `listen --json` -- WITHOUT touching the copy fed to
/// `manta_spot::Validator::ingest`, which already applies its own
/// calibration internally; applying it here too would double-correct the
/// validator's spot output (MAN-29 review round 3).
pub(crate) fn calibrate_track_meta(ev: &DecoderEvent, factor: f64) -> DecoderEvent {
    match ev {
        DecoderEvent::TrackMeta {
            track_id,
            snr_2500_db,
            freq_hz,
        } => DecoderEvent::TrackMeta {
            track_id: *track_id,
            snr_2500_db: *snr_2500_db,
            freq_hz: freq_hz * factor,
        },
        other => other.clone(),
    }
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
}

/// Deterministically pick the single `track_id` whose event slice becomes
/// `DecodeReport`'s headline `.text`/`.wpm`/`.freq_hz`.
///
/// MAN-3: the previous rule -- lowest `track_id` present -- silently
/// discarded a complete decode whenever an earlier, doomed track (promoted,
/// emitted one or two periodic `TrackMeta`, then closed before its own
/// `SpeedTracker` reached SPEC §4.1's 5-mark quorum) held a lower id than
/// the track that actually decoded the signal. `TrackManager::spawn`'s
/// `next_id` is spawn-ordered and has no relation to which track captures
/// the real signal; a short, fast signal routinely produces several
/// short-lived tracks in succession for one physical carrier (measured:
/// `track_ids: [6, 16]` for the ticket's "D5" case, both decoding the same
/// looped text). The correct decode was never lost -- it stayed in
/// `DecodeReport::events` -- it was just excluded from `.text`.
///
/// Rule: most `CharDecoded` events wins; ties break to the lowest
/// `track_id`. The all-tied-at-zero case (no track decoded anything) is a
/// tie, so a telemetry-only stream still reports the lowest id exactly as
/// before. `BTreeMap` gives a fixed ascending scan and the key is a total
/// order, so the result depends only on the event multiset -- not on
/// iteration order (SPEC §8 determinism).
fn select_report_track(events: &[DecoderEvent]) -> u32 {
    let mut chars_per_track: BTreeMap<u32, usize> = BTreeMap::new();
    for e in events {
        let entry = chars_per_track.entry(track::event_track_id(e)).or_insert(0);
        if matches!(e, DecoderEvent::CharDecoded { .. }) {
            *entry += 1;
        }
    }
    chars_per_track
        .into_iter()
        .max_by_key(|&(id, chars)| (chars, std::cmp::Reverse(id)))
        .map(|(id, _)| id)
        .expect("select_report_track requires a non-empty event stream")
}

/// M0 pipeline: estimate frequency, extract one channel, decode. SPEC
/// §1.3–§1.4, §3–§5.
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    // Validated up front so a bad config value (NaN, infinite, or ppm so
    // negative it flips the factor negative/zero) fails fast rather than
    // after the channelizer/decoder work below (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }

    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channelizer")?;
    let hop = ch.hop() as u64;

    debug_assert!(
        (fs / hop as f64 - manta_decode::FO_HZ).abs() < 0.01,
        "channelizer hop rate {} Hz diverges from manta_decode::FO_HZ {}",
        fs / hop as f64,
        manta_decode::FO_HZ
    );

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut tm = track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    // Feed the channelizer/track-manager in bounded chunks rather than one
    // `ch.process(&padded_iq)` + one `tm.process_hops(...)` call over the
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
    // continuously-decoding track's GC timer never falsely expires.
    const CHUNK_SAMPLES: usize = 4096;
    let mut events = Vec::new();
    let mut total_hops: u64 = 0;
    for chunk in padded_iq.chunks(CHUNK_SAMPLES) {
        let hops = ch.process(chunk);
        total_hops += hops.len() as u64;
        events.extend(tm.process_hops(&hops, |m| (m.saturating_sub(pad_hops)) * hop));
    }
    if total_hops == 0 {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    events.extend(tm.finish());

    // MAN-3: distinct from the length bail above -- the channelizer ran fine
    // and produced hops, but no track ever promoted *and* emitted. That is a
    // detection outcome (signal below `detector.on_snr_db`, or entirely
    // inside SPEC §2.1's warmup+confirm floor), not an input-length problem,
    // and reporting it as one sent this ticket's investigation down the
    // wrong path once already.
    if events.is_empty() {
        bail!(
            "no signal found: {total_hops} hops processed, but no track was ever \
             promoted and emitted (SPEC §2.4) -- signal below detector.on_snr_db, \
             or shorter than the ~2.05 s warmup+confirm floor"
        );
    }
    let report_track_id = select_report_track(&events);
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| track::event_track_id(e) == report_track_id)
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
    // Re-derives the same factor already validated above -- kept behind
    // the ppm-based constructor rather than a raw-factor setter so no
    // caller of manta-spot's public API can smuggle in an unchecked
    // factor (MAN-29 review: validate before construction, not after).
    let mut validator = manta_spot::Validator::bundled(fs)
        .with_freq_correction_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?
        .with_blocklist(cfg.blocklist.clone())
        .with_notch(cfg.notch.clone());
    for call in &cfg.allowlist {
        validator.allowlist(call);
    }
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
        if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
            *freq_hz *= calibration_factor;
        }
    }
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
        spots,
    })
}

/// decode_samples, sourced from a WAV file via manta-input. ARCHITECTURE
/// §3; SPEC §3–§5.
pub fn decode_wav(path: &Path, cfg: &PipelineConfig) -> Result<DecodeReport> {
    let mut src = WavIqSource::open(path)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    let iq = read_all(&mut src)?;
    decode_samples(&iq, fs, center, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_decode::tree::Glyph;

    fn meta(track_id: u32) -> DecoderEvent {
        DecoderEvent::TrackMeta {
            track_id,
            snr_2500_db: 20.0,
            freq_hz: 14_000.0,
        }
    }
    fn ch(track_id: u32, sample_ts: u64) -> DecoderEvent {
        DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            glyph: Glyph::Char('A'),
            confidence: 0.9,
        }
    }

    /// MAN-3, the "D5" shape: track 6 emits telemetry then dies; track 16
    /// carries the real decode. The old `min_track_id` rule reported track
    /// 6's empty history.
    #[test]
    fn report_track_is_the_one_that_decoded_not_the_lowest_id() {
        let events = vec![meta(6), meta(6), ch(16, 100), ch(16, 200), meta(16)];
        assert_eq!(select_report_track(&events), 16);
    }

    /// Ties (including the all-zero case: no track decoded anything
    /// anywhere) break to the lowest id -- preserving the historical
    /// behaviour exactly where the old rule was not wrong, so a
    /// telemetry-only stream still reports a stable, lowest-id track.
    #[test]
    fn report_track_ties_break_to_the_lowest_id() {
        assert_eq!(select_report_track(&[meta(9), meta(4), meta(7)]), 4);
        assert_eq!(select_report_track(&[ch(9, 1), ch(4, 2), meta(7)]), 4);
    }

    /// SPEC §8: selection must be a pure function of the event stream, with
    /// no dependence on iteration/insertion order.
    #[test]
    fn report_track_selection_is_order_independent() {
        let mut events = vec![meta(6), ch(16, 100), ch(16, 200), meta(6), meta(16)];
        let first = select_report_track(&events);
        events.reverse();
        assert_eq!(select_report_track(&events), first);
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
