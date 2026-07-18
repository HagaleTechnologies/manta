//! M1 streaming pipeline: live/replayed audio -> single channel -> decoder,
//! run continuously until Ctrl-C or EOF. No actor/ring-thread split -- M1
//! has exactly one track; see design doc §4.

use crate::PipelineConfig;
use anyhow::{Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::TrackDecoder;
use skimmer_decode::events::DecoderEvent;
use skimmer_dsp::freqest::estimate_peak_hz;
use skimmer_dsp::single::SingleChannelExtractor;
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
    let offset_hz =
        estimate_peak_hz(&calib, fs).context("no signal found during startup calibration")?;

    let mut extractor =
        SingleChannelExtractor::new(fs, offset_hz).map_err(|e| anyhow::anyhow!(e))?;
    let hop = extractor.hop() as u64;
    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(offset_hz);

    // Same lead-in fix as the M0 batch path (extractor group-delay blind
    // zone), applied once at stream start instead of per-file: prime the
    // extractor with one filter length of zero IQ before real audio, and
    // feed every resulting output (do not skip -- see M0 pinned decision 19).
    let pad_samples = extractor.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    let mut m: u64 = 0;
    for y in extractor.process(&padding) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(y.norm(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }
    // Feed the calibration window too -- it was already consumed from the
    // source and must not be discarded.
    for y in extractor.process(&calib) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(y.norm(), sample_ts) {
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
            break; // EOF (file replay only; live sources block instead)
        }
        for y in extractor.process(&chunk[..n]) {
            let sample_ts = m.saturating_sub(pad_hops) * hop;
            for ev in decoder.push_envelope(y.norm(), sample_ts) {
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
