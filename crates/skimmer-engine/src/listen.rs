//! Streaming pipeline: live/replayed audio -> PFB channelizer ->
//! `TrackManager` (SPEC §2), run continuously until Ctrl-C or EOF, emitting
//! the merged multi-track decode event stream as it's produced. No actor/
//! ring-thread split; see design doc §4.

use crate::PipelineConfig;
use anyhow::Result;
use num_complex::Complex32;
use skimmer_decode::events::DecoderEvent;
use skimmer_input::IqSource;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
) -> Result<()> {
    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();

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
    let mut ch = skimmer_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&ev);
    }
    for ev in tm.process_hops(&ch.process(&calib), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&ev);
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
        for ev in tm.process_hops(&ch.process(&chunk[..n]), |m| {
            m.saturating_sub(pad_hops) * hop
        }) {
            on_event(&ev);
        }
    }
    for ev in tm.finish() {
        on_event(&ev);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl skimmer_input::IqSource for FixedFreqSource {
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

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        let spec = skimmer_testkit::vectors::v1();
        let rendered = skimmer_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn skimmer_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(src, &PipelineConfig::default(), stop, |ev| {
            if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                last_freq_hz = Some(*freq_hz);
            }
        })
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340",
            spec.center_freq_hz + 12_340.0
        );
    }
}
