//! Live/replayed real-audio IQ source: coppa-audio AudioSource -> Hilbert
//! transformer -> Complex32, matching IqSource. ARCHITECTURE §3, design
//! doc §2.
//!
//! M1 scope: no automatic resampling. Sources must already run at exactly
//! TARGET_RATE_HZ (48000 Hz) natively; a rate mismatch is a hard error, not
//! a resample attempt (coppa-audio's ResamplingSource is unreachable -- no
//! `rubato` dependency and no `mod resampler;` declaration upstream).

use crate::IqSource;
use anyhow::{anyhow, Context, Result};
use coppa_audio::AudioSource;
use num_complex::Complex32;
use skimmer_dsp::hilbert::HilbertTransformer;
use std::path::Path;

/// Fixed target sample rate for M1 audio decode: 48000 / 93.75 = 512 (a
/// power of two), the constraint SingleChannelExtractor::new requires.
pub const TARGET_RATE_HZ: u32 = 48_000;

/// A real audio source (device or file) converted to analytic Complex32,
/// implementing IqSource. ARCHITECTURE §3 "Audio passband" input.
pub struct AudioIqSource {
    src: Box<dyn AudioSource>,
    hilbert: HilbertTransformer,
}

impl AudioIqSource {
    /// Wrap an already-started AudioSource at TARGET_RATE_HZ.
    pub fn new(src: Box<dyn AudioSource>) -> Result<Self> {
        if src.sample_rate() != TARGET_RATE_HZ {
            return Err(anyhow!(
                "AudioIqSource requires {TARGET_RATE_HZ} Hz, got {}",
                src.sample_rate()
            ));
        }
        Ok(AudioIqSource {
            src,
            hilbert: HilbertTransformer::new(),
        })
    }

    /// Open the named input device (default device if `None`). Requires the
    /// device's native rate to be exactly TARGET_RATE_HZ (48000) -- M1 does
    /// not resample; see AudioIqSource::new for the rate-mismatch error.
    pub fn from_device(name: Option<&str>) -> Result<Self> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let device = match name {
            Some(n) => coppa_audio::find_input_device_by_name(n)
                .ok_or_else(|| anyhow!("no input device matching {n:?}"))?,
            None => cpal::default_host()
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };
        let native_rate = device
            .default_input_config()
            .context("query device default input config")?
            .sample_rate();
        let mut cpal_src = coppa_audio::CpalSource::from_device(device, native_rate, 8192)?;
        cpal_src.start()?;
        AudioIqSource::new(Box::new(cpal_src))
    }

    /// Open a WAV file, replayed as an audio source (soak harness / `listen
    /// --source`). Requires the file's rate to be exactly TARGET_RATE_HZ.
    pub fn from_wav_file(path: &Path) -> Result<Self> {
        let wav_src = coppa_audio::WavSource::open(path)?;
        AudioIqSource::new(Box::new(wav_src))
    }
}

impl IqSource for AudioIqSource {
    fn sample_rate(&self) -> f64 {
        TARGET_RATE_HZ as f64
    }

    fn center_freq_hz(&self) -> f64 {
        0.0 // audio has no RF reference; offset-only reporting (design doc §2)
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let got = self.src.read(&mut real)?;
        if got == 0 {
            return Ok(0);
        }
        let analytic = self.hilbert.process(&real[..got]);
        buf[..got].copy_from_slice(&analytic);
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_real_samples_to_analytic_iq() {
        let fs = TARGET_RATE_HZ;
        let f = 1_000.0;
        let samples: Vec<f32> = (0..4000)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs as f64).cos() as f32)
            .collect();
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(samples, fs));
        let mut aiq = AudioIqSource::new(src).unwrap();
        assert_eq!(aiq.sample_rate(), fs as f64);
        assert_eq!(aiq.center_freq_hz(), 0.0);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4000];
        let n = aiq.read(&mut buf).unwrap();
        assert!(n > 0);
        // Well past the Hilbert filter's transient, magnitude should be ~unit.
        assert!(
            (buf[2000].norm() - 1.0).abs() < 0.1,
            "norm={}",
            buf[2000].norm()
        );
    }

    #[test]
    fn rejects_mismatched_sample_rate() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 10], 44_100));
        assert!(AudioIqSource::new(src).is_err());
    }

    #[test]
    fn reports_eof_as_zero_read() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 5], TARGET_RATE_HZ));
        let mut aiq = AudioIqSource::new(src).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(aiq.read(&mut buf).unwrap(), 5);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
    }
}
