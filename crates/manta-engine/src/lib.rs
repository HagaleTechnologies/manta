//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod listen;
pub use listen::listen;
pub mod doctor;
pub use doctor::{doctor, DoctorReport, Verdict, MAX_DURATION, MIN_DURATION};
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
            snr_2500_db,
            freq_hz,
        } => DecoderEvent::TrackMeta {
            track_id: *track_id,
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

/// The frequency `DecodeReport` should carry for `select_report_track`'s
/// chosen track: its own most recent `TrackMeta` centroid, else its own
/// promotion-time centroid (`TrackPromoted.freq_hz`), else the receiver
/// center.
///
/// Review round 2 introduced a middle fallback that reused the most recent
/// `TrackMeta` from *any* track, because `select_report_track` can pick a
/// late, short-lived replacement that emitted `CharDecoded` before ever
/// reaching the 375-hop (1 Hz) `TrackMeta` cadence, leaving it with no
/// metadata of its own. Round 6 removed that: in a passband carrying
/// several simultaneous signals the last `TrackMeta` from *any* track can
/// belong to a completely different carrier, which would pair this track's
/// text with another signal's frequency in `DecodeReport::freq_hz` and in
/// the CLI/JSON headline.
///
/// The metadata-less case no longer needs a cross-track guess: every
/// promoted track now emits its own `TrackPromoted` carrying the centroid
/// measured for *this* carrier at its promotion hop
/// (docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md), and a track
/// cannot decode a character without having been promoted first. So the
/// selected track always has its own measurement to fall back on, and
/// `center_freq_hz` stays the last-resort default only for a stream that
/// carries neither event kind for it.
fn report_freq_hz(this_track: &[DecoderEvent], center_freq_hz: f64) -> f64 {
    let last_meta_freq = this_track.iter().rev().find_map(|e| match e {
        DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
        _ => None,
    });
    last_meta_freq
        .or_else(|| {
            this_track.iter().rev().find_map(|e| match e {
                DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
                _ => None,
            })
        })
        .unwrap_or(center_freq_hz)
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
/// Rule: most *rendered* characters wins; ties break to the lowest
/// `track_id`. The all-tied-at-zero case (no track decoded anything) is a
/// tie, so a telemetry-only stream still reports the lowest id exactly as
/// before. `BTreeMap` gives a fixed ascending scan and the key is a total
/// order, so the result depends only on the event multiset -- not on
/// iteration order (SPEC §8 determinism).
///
/// "Rendered" is `Glyph::text_char().is_some()`, i.e. exactly the events
/// `events_to_text` keeps: SPEC §4.4 drops every `Glyph::Prosign` from
/// telnet-facing text, so a track whose only `CharDecoded` events are
/// prosigns (`<AR>`, `<SK>`, ...) contributes *nothing* to `.text`.
/// Counting raw `CharDecoded` would let such a track out-rank one carrying
/// real letters and hand `.text` back an empty string -- the very failure
/// this selector exists to prevent (review round 1).
///
/// Only tracks with at least one *decoder* event are candidates.
/// `TrackPromoted` is a detector-internal diagnostic signal (added for
/// `manta_engine::doctor()`'s NoSignal check, see
/// docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md) carrying no
/// decoder output of its own, so a candidate that promoted and was then
/// merged/evicted/EOF'd before producing anything must not enter the
/// ranking at all -- it would tie at zero rendered characters and win the
/// lowest-id tie-break over the track that actually decoded the signal
/// (round-5 review finding, previously `primary_track_id`). `None` means
/// no track produced any decoder output at all -- an empty `events`, or
/// one made up entirely of `TrackPromoted`.
fn select_report_track(events: &[DecoderEvent]) -> Option<u32> {
    let mut chars_per_track: BTreeMap<u32, usize> = BTreeMap::new();
    for e in events {
        if matches!(e, DecoderEvent::TrackPromoted { .. }) {
            continue;
        }
        let entry = chars_per_track.entry(track::event_track_id(e)).or_insert(0);
        if matches!(e, DecoderEvent::CharDecoded { glyph, .. } if glyph.text_char().is_some()) {
            *entry += 1;
        }
    }
    chars_per_track
        .into_iter()
        .max_by_key(|&(id, chars)| (chars, std::cmp::Reverse(id)))
        .map(|(id, _)| id)
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
    // detection outcome, not an input-length problem, and reporting it as one
    // sent this ticket's investigation down the wrong path once already.
    //
    // Review round 2: the two ways to reach zero events have *different*
    // causes and must not share one message. `promoted_count()` separates
    // them: zero promotions really is a detector outcome (below
    // `detector.on_snr_db`, or inside SPEC §2.1's warmup+confirm floor), but
    // a track that promoted and still emitted nothing was above threshold and
    // longer than that floor -- it just closed before clearing the decoder's
    // own output latency (the 1 Hz `TrackMeta` cadence, or §5's mark quorum
    // for a first character). Pointing an operator at detector thresholds for
    // that second case is actively misleading.
    //
    // Review round 3: the *prefix* has to split too. "no signal found" is
    // the one thing that is demonstrably untrue of a promoted track -- the
    // detector found a signal, held it above `on_snr_db` and confirmed it;
    // only the decoder had nothing to say before it closed. Opening that
    // branch with "no signal found" contradicts its own body ("N track(s)
    // promoted") and is exactly the misdirection the split exists to stop,
    // so the promoted branch leads with what actually happened.
    //
    // Merge with main's `doctor()` work: `events` is no longer empty in
    // either case -- every promotion now also puts a `TrackPromoted` on
    // the stream -- so the split keys off `select_report_track`, which
    // returns `None` for exactly the "no *decoder* output anywhere"
    // condition `events.is_empty()` used to stand for.
    let Some(report_track_id) = select_report_track(&events) else {
        let promoted = tm.promoted_count();
        if promoted == 0 {
            bail!(
                "no signal found: {total_hops} hops processed, but no track was ever \
                 promoted (SPEC §2.4) -- signal below detector.on_snr_db, or shorter \
                 than the ~2.05 s warmup+confirm floor"
            );
        }
        bail!(
            "signal detected but nothing decoded: {total_hops} hops processed and \
             {promoted} track(s) promoted above detector.on_snr_db, but none emitted \
             before closing -- a promoted track needs a further ~1 s of decoding to \
             reach its first 1 Hz TrackMeta, or enough marks for a first character \
             (SPEC §2.4/§5); this is decoder-output latency, not a detector-threshold \
             problem"
        );
    };
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| track::event_track_id(e) == report_track_id)
        .cloned()
        .collect();
    // `events` is still uncalibrated here (the `calibration_factor` pass over
    // it runs below), so `report_freq_hz` sees raw centroids and the single
    // multiply below applies the correction once.
    let freq_hz = report_freq_hz(&this_track, center_freq_hz) * calibration_factor;
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
        assert_eq!(select_report_track(&events), Some(16));
    }

    /// Ties (including the all-zero case: no track decoded anything
    /// anywhere) break to the lowest id -- preserving the historical
    /// behaviour exactly where the old rule was not wrong, so a
    /// telemetry-only stream still reports a stable, lowest-id track.
    #[test]
    fn report_track_ties_break_to_the_lowest_id() {
        assert_eq!(select_report_track(&[meta(9), meta(4), meta(7)]), Some(4));
        assert_eq!(select_report_track(&[ch(9, 1), ch(4, 2), meta(7)]), Some(4));
    }

    /// Review round 1: `events_to_text` drops every `Glyph::Prosign` (SPEC
    /// §4.4), so a prosign-only track renders to an empty string. Ranking
    /// raw `CharDecoded` counts would hand it `.text` over a track carrying
    /// real letters -- reproducing the empty-`.text` failure this selector
    /// exists to prevent.
    #[test]
    fn report_track_ranks_only_glyphs_that_render_into_text() {
        let prosign = |track_id: u32, sample_ts: u64| DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            glyph: Glyph::Prosign(manta_decode::tree::Prosign::Ar),
            confidence: 0.9,
        };
        let events = vec![
            prosign(3, 10),
            prosign(3, 20),
            prosign(3, 30),
            ch(7, 100),
            meta(7),
        ];
        assert_eq!(select_report_track(&events), Some(7));
        assert_eq!(events_to_text(&events), "A");
    }

    /// MAN-3 review round 6: the reported track can be a late, short-lived
    /// replacement that decoded characters but never reached its own 1 Hz
    /// `TrackMeta` cadence. Its frequency must come from its OWN
    /// promotion-time centroid, never from another track's metadata --
    /// in a multi-signal passband that other track is a different carrier,
    /// and reporting its frequency would pair this track's text with the
    /// wrong signal.
    #[test]
    fn report_freq_uses_the_selected_tracks_own_promotion_centroid() {
        let other_carrier = DecoderEvent::TrackMeta {
            track_id: 6,
            snr_2500_db: 20.0,
            freq_hz: 21_000.0,
        };
        let promoted = DecoderEvent::TrackPromoted {
            track_id: 16,
            sample_ts: 50,
            freq_hz: 14_000.0,
        };
        let events = vec![other_carrier, promoted.clone(), ch(16, 100), ch(16, 200)];
        let selected = select_report_track(&events).unwrap();
        assert_eq!(selected, 16);
        let this_track: Vec<DecoderEvent> = events
            .iter()
            .filter(|e| track::event_track_id(e) == selected)
            .cloned()
            .collect();
        assert!(this_track
            .iter()
            .all(|e| !matches!(e, DecoderEvent::TrackMeta { .. })));
        assert_eq!(report_freq_hz(&this_track, 7_000.0), 14_000.0);
    }

    /// The selected track's own metadata still wins over its own (older)
    /// promotion centroid, and a track carrying neither still falls back
    /// to the receiver center.
    #[test]
    fn report_freq_prefers_the_selected_tracks_own_metadata() {
        let this_track = vec![
            DecoderEvent::TrackPromoted {
                track_id: 16,
                sample_ts: 10,
                freq_hz: 13_000.0,
            },
            meta(16),
            ch(16, 100),
        ];
        assert_eq!(report_freq_hz(&this_track, 7_000.0), 14_000.0);
        assert_eq!(report_freq_hz(&[ch(16, 100)], 7_000.0), 7_000.0);
        assert_eq!(report_freq_hz(&[], 7_000.0), 7_000.0);
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

    /// Regression (round-5 review, P1): an early low-track-id candidate
    /// that only ever produced a `TrackPromoted` (promoted, then merged/
    /// evicted/EOF'd before any real decoder output) must never be
    /// selected over a later track that actually decoded something.
    #[test]
    fn report_track_skips_a_promotion_only_track() {
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
                snr_2500_db: 20.0,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 2,
                sample_ts: 20,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
            },
        ];
        assert_eq!(select_report_track(&events), Some(2));
    }

    #[test]
    fn report_track_is_none_when_only_promotions_occurred() {
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
        assert_eq!(select_report_track(&events), None);
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
