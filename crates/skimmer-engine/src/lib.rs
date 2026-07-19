//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

pub mod listen;
pub use listen::listen;
pub mod soak;
pub use soak::{soak, soak_passed, SoakReport};

mod detect;

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use skimmer_decode::events::DecoderEvent;
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

    // Calibration pass (design doc §2): find the loudest channel over a
    // fixed-length window, matching the M0/M1 approach of a one-time
    // startup estimate. Reuse the whole input for calibration, same as M0's
    // decode_samples always did (batch mode has the whole file up front).
    let Some(k0) = crate::detect::calibrate_channel(&mut ch, iq) else {
        bail!("no signal found (input shorter than one filter length or empty)");
    };
    let offset_hz = ch.channel_freq_hz(k0) - center_freq_hz;

    // Same lead-in group-delay fix as the M0 shim (pinned decision 19):
    // the channelizer's causal FIR window has an identical blind-zone
    // property at stream start. Re-run calibration's channel choice, but
    // reprocess from a padded, freshly-constructed channelizer so every
    // hop (including the padding/real-signal boundary) is fed to the
    // decoder -- calibrate_channel already consumed `iq` once above just to
    // pick k0; that channelizer instance is discarded, not reused, so this
    // second pass starts from a clean internal buffer.
    let mut ch = skimmer_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(center_freq_hz + offset_hz);

    // SPEC §1.4 fine-frequency track centroid: over hops where k0 is a
    // local max in dB power (interpolate_offset returns Some), accumulate
    // a power-weighted sub-channel offset. This refines only the FINAL
    // REPORTED freq_hz below -- it must NOT change what feeds
    // TrackDecoder::push_envelope, since decode accuracy (text/WPM) doesn't
    // depend on frequency precision, only the reported spot frequency does.
    let n = ch.n_channels();
    let k_minus = (k0 + n - 1) % n;
    let k_plus = (k0 + 1) % n;
    let mut sum_weighted = 0.0f64;
    let mut sum_power = 0.0f64;

    let hops = ch.process(&padded_iq);
    let mut events: Vec<DecoderEvent> = Vec::new();
    for hop_out in &hops {
        let sample_ts = hop_out.m.saturating_sub(pad_hops) * hop;
        let mag = hop_out.power[k0].sqrt();
        events.extend(decoder.push_envelope(mag, sample_ts));

        if let Some(delta) = skimmer_dsp::channelizer::interpolate_offset(
            hop_out.power[k_minus],
            hop_out.power[k0],
            hop_out.power[k_plus],
        ) {
            let w = hop_out.power[k0] as f64;
            sum_weighted += (k0 as f64 + delta) * w;
            sum_power += w;
        }
    }
    events.extend(decoder.finish());

    let centroid = if sum_power > 0.0 {
        sum_weighted / sum_power
    } else {
        k0 as f64
    };
    let fine_freq_hz = center_freq_hz + (centroid - k0 as f64) * (fs / n as f64) + offset_hz;

    let wpm = events.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&events);
    Ok(DecodeReport {
        freq_hz: fine_freq_hz,
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
