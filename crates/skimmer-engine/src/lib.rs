//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod listen;
pub use listen::listen;
pub mod soak;
pub use soak::{soak, soak_passed, SoakReport};

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use skimmer_decode::events::DecoderEvent;
use skimmer_dsp::freqest::estimate_peak_hz;
use skimmer_dsp::single::SingleChannelExtractor;
use skimmer_input::{read_all, IqSource, WavIqSource};
use std::path::Path;

/// M0 pipeline tunables. SPEC §5.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    pub decode: DecodeConfig,
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
}

/// M0 pipeline: estimate frequency, extract one channel, decode. SPEC
/// §1.3–§1.4, §3–§5.
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    // Degenerate-input guard: a flat spectrum yields a meaningless argmax,
    // and the extractor + demod pre-decode gate would just produce no
    // output anyway. Check this FIRST, before spending time on the
    // periodogram in estimate_peak_hz below — bail out immediately on
    // all-zero input instead of running the FFT peak search on it.
    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }
    let Some(offset_hz) = estimate_peak_hz(iq, fs) else {
        bail!("no signal found (input shorter than one FFT frame or empty)");
    };

    let mut extractor = SingleChannelExtractor::new(fs, offset_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channel extractor")?;
    let hop = extractor.hop() as u64;

    // Cross-crate invariant: skimmer-decode hardcodes FO_HZ = 375.0 and
    // HOP_MS = 8.0/3.0 (SPEC §3.1) on the assumption that the extractor's
    // channel output rate is exactly 375 Hz. skimmer-dsp derives that rate
    // independently from fs / CHANNEL_SPACING_HZ / TAPS_PER_BRANCH
    // (single.rs), with nothing tying the two together — if either crate's
    // constants ever changed, this would silently desync (decode.rs's
    // duration math would go quietly wrong instead of failing loudly).
    // This check is expected to pass silently for all supported rates.
    debug_assert!(
        (fs / hop as f64 - skimmer_decode::FO_HZ).abs() < 0.01,
        "extractor hop rate {} Hz diverges from skimmer_decode::FO_HZ {}",
        fs / hop as f64,
        skimmer_decode::FO_HZ
    );

    // The extractor's causal FIR channel filter has a fixed group delay of
    // (filter_len()-1)/2 samples: no output hop can ever represent a true
    // signal instant earlier than that into a recording with no prior
    // history, so a message keyed with no lead-in silence has its opening
    // element(s) corrupted or destroyed outright. Fix: prepend synthetic
    // zero-valued lead-in samples (one full filter length, comfortably more
    // than the exact group-delay minimum) so the filter's blind zone
    // consumes only harmless padding, giving it genuine (zero) pre-history
    // it never had before; then re-baseline sample_ts back onto the
    // original, unpadded input-stream sample counter (SPEC §5). See
    // .superpowers/sdd/task-14-investigation-report.md.
    let pad_samples = extractor.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(center_freq_hz + offset_hz);

    // Feed every output the padded stream produces, including those whose
    // analysis window straddles the padding/real-signal boundary: those
    // windows mix genuine (zero) pre-history with the real signal's true
    // onset and are exactly what recovers the message's opening element(s)
    // from the extractor's blind zone (see the investigation report).
    // Discarding them (skipping m < pad_hops) would reproduce the original
    // bug exactly: window start for output m is m*hop, and pad_hops*hop ==
    // pad_samples by construction, so output pad_hops's window starts
    // exactly where the real samples begin. That window is NOT byte-
    // identical to the unpadded extractor's own first output, though —
    // process() mixes by an absolute-index-dependent NCO phase before
    // filtering, so the two windows differ by a global complex phase factor.
    // But the FIR is linear, so that phase factor scales the whole output
    // uniformly, and decode reads y.norm() (magnitude only, phase discarded)
    // — so the two envelope magnitudes ARE identical, which is what actually
    // makes "skip past the padding" a proven no-op. Rebaseline sample_ts back onto
    // the ORIGINAL (unpadded) input-stream counter for SPEC §5, saturating
    // at 0 for hops whose window falls at or before the true recording
    // start — decode correctness does not depend on sample_ts (durations
    // are computed from run.hops, a hop count, not from sample_ts
    // differences; decoder.rs `let dur_ms = run.hops as f32 * HOP_MS`), only
    // downstream reporting does.
    let channel = extractor.process(&padded_iq);
    let mut events: Vec<DecoderEvent> = Vec::new();
    for (m, y) in channel.iter().enumerate() {
        let m = m as u64;
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        events.extend(decoder.push_envelope(y.norm(), sample_ts));
    }
    events.extend(decoder.finish());

    let wpm = events.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&events);
    Ok(DecodeReport {
        freq_hz: center_freq_hz + offset_hz,
        wpm,
        text,
        events,
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
