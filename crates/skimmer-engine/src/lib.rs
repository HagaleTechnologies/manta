//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod listen;
pub use listen::listen;
pub mod soak;
pub use skimmer_spot::{Spot, SpotType};
pub use soak::{soak, soak_passed, SoakReport};

mod track;
pub use track::DetectorConfig;

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::{events_to_text, DecodeConfig};
use skimmer_decode::events::DecoderEvent;
use skimmer_input::{read_all, IqSource, WavIqSource};
use std::path::Path;

/// M0 pipeline tunables. SPEC §5.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    /// Classical decoder tunables. SPEC §5.
    pub decode: DecodeConfig,
    /// Real multi-track detector tunables. SPEC §9 `[detector]` table.
    pub detector: track::DetectorConfig,
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
    /// Validated spots (`skimmer-spot::Validator`, ARCHITECTURE §6), run
    /// over the full multi-track event stream above.
    pub spots: Vec<Spot>,
}

/// M0 pipeline: estimate frequency, extract one channel, decode. SPEC
/// §1.3–§1.4, §3–§5.
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }

    let mut ch = skimmer_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channelizer")?;
    let hop = ch.hop() as u64;

    debug_assert!(
        (fs / hop as f64 - skimmer_decode::FO_HZ).abs() < 0.01,
        "channelizer hop rate {} Hz diverges from skimmer_decode::FO_HZ {}",
        fs / hop as f64,
        skimmer_decode::FO_HZ
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
    let mut any_hops = false;
    for chunk in padded_iq.chunks(CHUNK_SAMPLES) {
        let hops = ch.process(chunk);
        any_hops |= !hops.is_empty();
        events.extend(tm.process_hops(&hops, |m| (m.saturating_sub(pad_hops)) * hop));
    }
    if !any_hops {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    events.extend(tm.finish());

    if events.is_empty() {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    let min_track_id = events.iter().map(track::event_track_id).min().unwrap();
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| track::event_track_id(e) == min_track_id)
        .cloned()
        .collect();
    let freq_hz = this_track
        .iter()
        .rev()
        .find_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .unwrap_or(center_freq_hz);
    let wpm = this_track.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&this_track);
    let mut validator = skimmer_spot::Validator::bundled(fs);
    let mut spots = Vec::new();
    for ev in &events {
        spots.extend(validator.ingest(ev));
    }
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
        spots,
    })
}

/// decode_samples, sourced from a WAV file via skimmer-input. ARCHITECTURE
/// §3; SPEC §3–§5.
pub fn decode_wav(path: &Path, cfg: &PipelineConfig) -> Result<DecodeReport> {
    let mut src = WavIqSource::open(path)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    let iq = read_all(&mut src)?;
    decode_samples(&iq, fs, center, cfg)
}
