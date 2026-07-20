//! M1 streaming pipeline: live/replayed audio -> single channel -> decoder,
//! run continuously until Ctrl-C or EOF. No actor/ring-thread split -- M1
//! has exactly one track; see design doc §4.

use crate::PipelineConfig;
use anyhow::{Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::TrackDecoder;
use skimmer_decode::events::DecoderEvent;
use skimmer_input::{AudioIqSource, IqSource};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered to estimate the initial channel offset before
/// the extractor is built -- a fixed startup calibration, not re-estimated
/// afterward (M1 has no PFB/track manager to re-tune mid-stream).
const CALIBRATION_SECONDS: f64 = 2.0;

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
pub fn listen(
    mut src: AudioIqSource,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
) -> Result<()> {
    let fs = src.sample_rate();

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
    let mut calib_ch =
        skimmer_dsp::channelizer::Channelizer::new(fs, 0.0).map_err(|e| anyhow::anyhow!(e))?;
    let k0 = calibrate_channel(&mut calib_ch, &calib)
        .context("no signal found during startup calibration")?;
    let mut ch =
        skimmer_dsp::channelizer::Channelizer::new(fs, 0.0).map_err(|e| anyhow::anyhow!(e))?;
    let offset_hz = ch.channel_freq_hz(k0);
    let hop = ch.hop() as u64;
    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(offset_hz);

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    let mut m: u64 = 0;
    for hop_out in ch.process(&padding) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }
    for hop_out in ch.process(&calib) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        for hop_out in ch.process(&chunk[..n]) {
            let sample_ts = m.saturating_sub(pad_hops) * hop;
            for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
                on_event(&ev);
            }
            m += 1;
        }
    }
    for ev in decoder.finish() {
        on_event(&ev);
    }
    Ok(())
}

/// Placeholder-detector calibration helper (design doc §2): run `ch` over
/// `calib_iq` and return the channel index with the highest average power
/// across the resulting hops, or `None` if no hops were produced
/// (calibration window shorter than one filter length). Formerly lived in
/// `detect.rs` as the M0/M1 shim's only consumer-agnostic piece; `listen()`
/// above is now its sole caller (`decode_samples`/`decode_wav` moved onto
/// the real `track::TrackManager` in Task 7) -- kept here only until Task 8
/// wires `listen()` onto `TrackManager` too, at which point this helper goes
/// away entirely.
fn calibrate_channel(
    ch: &mut skimmer_dsp::channelizer::Channelizer,
    calib_iq: &[Complex32],
) -> Option<usize> {
    let hops = ch.process(calib_iq);
    if hops.is_empty() {
        return None;
    }
    let n = ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in &hops {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    let mut k0 = 0;
    for (k, &p) in avg_power.iter().enumerate() {
        if p > avg_power[k0] {
            k0 = k;
        }
    }
    Some(k0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skimmer_dsp::channelizer::Channelizer;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn finds_the_loudest_channel() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 96_000, 1.0, FS); // 1 s calibration window
        let k0 = calibrate_channel(&mut ch, &iq).unwrap();
        assert_eq!(k0, 64); // 6000 / 93.75 = 64 exactly
    }

    #[test]
    fn none_on_too_short_input() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 10, 1.0, FS); // far shorter than one filter length
        assert!(calibrate_channel(&mut ch, &iq).is_none());
    }
}
