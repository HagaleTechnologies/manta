//! Looping file replay (`--loop`, MAN-121).

use crate::{open_replay_wav, IqSource};
use anyhow::Result;
use num_complex::Complex32;
use std::path::PathBuf;

/// Reopens the file at EOF so replay never ends.
///
/// Reopening (rather than rewinding) is what lets this work for BOTH
/// replay flavours -- `WavIqSource` holds a cursor with no public rewind,
/// and `AudioIqSource` wraps an opaque `coppa_audio::AudioSource` with no
/// rewind of its own either.
///
/// The wrap point is a hard discontinuity in the sample stream (last
/// sample straight to first), which the channelizer sees as a click.
/// Harmless for a demo; this is not a substitute for a genuinely long
/// recording.
pub struct LoopingWavSource {
    inner: Box<dyn IqSource>,
    path: PathBuf,
}

impl LoopingWavSource {
    pub fn new(path: PathBuf) -> Result<Self> {
        let inner = open_replay_wav(&path)?;
        Ok(LoopingWavSource { inner, path })
    }
}

impl IqSource for LoopingWavSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        self.inner = open_replay_wav(&self.path)?;
        self.inner.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_f32_wav(path: &std::path::Path, samples: &[Complex32], fs: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: fs,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(s.re).unwrap();
            w.write_sample(s.im).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn loops_past_end_of_file_reproducing_the_same_samples() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("short.wav");
        let samples: Vec<Complex32> = (0..10)
            .map(|i| Complex32::new(i as f32, -(i as f32)))
            .collect();
        write_f32_wav(&wav, &samples, 8000);

        let mut src = LoopingWavSource::new(wav).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 10];

        // First pass.
        assert_eq!(src.read(&mut buf).unwrap(), 10);
        assert_eq!(buf, samples);

        // EOF triggers a reopen; the second pass reproduces the same data
        // rather than returning 0 and stopping.
        assert_eq!(src.read(&mut buf).unwrap(), 10);
        assert_eq!(buf, samples);
    }
}
