//! Live/replayed real-audio IQ source: coppa-audio AudioSource -> Hilbert
//! transformer -> Complex32, matching IqSource. ARCHITECTURE §3, design
//! doc §2.
//!
//! M1 scope: no automatic resampling. Sources must already run at exactly
//! TARGET_RATE_HZ (48000 Hz) natively; a rate mismatch is a hard error, not
//! a resample attempt (coppa-audio's ResamplingSource is unreachable -- no
//! `rubato` dependency and no `mod resampler;` declaration upstream).
//!
//! An RF reference is optional but supported: `with_center_freq_hz` accepts
//! an operator-supplied dial frequency (MAN-34), superseding design doc
//! §2's original "audio has no RF reference" scope decision. Without it,
//! `center_freq_hz()` still reports `0.0` and downstream frequencies are
//! bare baseband offsets.
//!
//! Live devices (`from_device`) sit behind coppa's non-blocking callback
//! ring, where an empty read means "nothing captured yet", not end of
//! stream. `read` therefore waits for samples, and fails after
//! `DEVICE_STALL_TIMEOUT` if none arrive, so the `IqSource` contract
//! `0 = EOF` holds for every source (MAN-231). File-backed sources keep
//! returning 0 at end of file.

use crate::IqSource;
use anyhow::{anyhow, bail, Context, Result};
use coppa_audio::AudioSource;
use manta_dsp::hilbert::HilbertTransformer;
use num_complex::Complex32;
use std::path::Path;
use std::time::{Duration, Instant};

/// Fixed target sample rate for M1 audio decode: 48000 / 93.75 = 512 (a
/// power of two), the constraint SingleChannelExtractor::new requires.
pub const TARGET_RATE_HZ: u32 = 48_000;

/// The rig-audio passband this source actually delivers, as offsets in Hz
/// ABOVE the dial frequency (`--dial-freq-hz`).
///
/// MAN-86 review: these are what `SKIMMER/SETT` advertises to Aggregator as
/// decodable coverage, and they are deliberately NOT derived from
/// `TARGET_RATE_HZ`. Two independent reasons: `read` Hilbert-transforms
/// real audio into an analytic signal, so all of its energy sits at
/// POSITIVE offsets from the dial -- the negative half of a
/// `+/- TARGET_RATE_HZ / 2` claim is on the wrong side of the dial
/// entirely; and a receiver's AF output is the ~3 kHz passband its IF
/// filter passes (ARCHITECTURE §3, "Degenerate ~3 kHz wideband mode"), not
/// the 24 kHz its sound card happens to sample.
///
/// The values are the nominal SSB/CW-wide AF passband a rig in its widest
/// setting delivers. A rig running a narrow CW filter passes less than
/// this, so the claim is a superset for that operator; making it exact
/// needs a config key for the rig's actual filter, which is out of MAN-86's
/// scope and is why these are named constants rather than literals.
pub const AUDIO_PASSBAND_LO_HZ: f64 = 300.0;
/// Upper edge of the rig-audio passband -- see `AUDIO_PASSBAND_LO_HZ`.
pub const AUDIO_PASSBAND_HI_HZ: f64 = 3_000.0;

/// How long a live device's `read` waits for a sample before failing
/// (MAN-231). Below the ~10 s network-source bound (hpsdr.rs/kiwi.rs):
/// local audio callbacks arrive every ~10 ms, and an operator whose device
/// delivers nothing should hear so quickly. A read in flight is not
/// interruptible, so this also bounds Ctrl-C latency during a stall; on a
/// delivering device each read returns within one callback period.
pub const DEVICE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Sleep between polls of an empty capture ring (matches `manta check`).
/// The ring holds 8192 samples, ~170 ms at 48 kHz, so a 10 ms poll cannot
/// let it overflow.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A capture device behind a non-blocking callback ring (coppa's
/// `CpalSource`): `read` must wait for samples, because an empty ring is
/// not end of stream.
struct LiveDevice {
    stall_timeout: Duration,
}

/// A real audio source (device or file) converted to analytic Complex32,
/// implementing IqSource. ARCHITECTURE §3 "Audio passband" input.
pub struct AudioIqSource {
    src: Box<dyn AudioSource>,
    hilbert: HilbertTransformer,
    /// Operator-supplied RF dial frequency this audio passband sits on, Hz.
    /// `0.0` = no RF reference supplied: reported frequencies are baseband
    /// offsets. See `with_center_freq_hz` (MAN-34).
    center_freq_hz: f64,
    /// `Some` only for `from_device`; file sources keep `0 = EOF`.
    live: Option<LiveDevice>,
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
            center_freq_hz: 0.0,
            live: None,
        })
    }

    /// Wrap a started live device: `read` waits up to `stall_timeout` for
    /// samples instead of reporting an empty ring as end of stream.
    fn live(src: Box<dyn AudioSource>, stall_timeout: Duration) -> Result<Self> {
        let mut source = Self::new(src)?;
        source.live = Some(LiveDevice { stall_timeout });
        Ok(source)
    }

    /// Attach the operator-supplied RF dial frequency the rig is tuned to
    /// (MAN-34). An audio passband carries no RF reference of its own, so
    /// without this a track's reported frequency is a bare audio-tone
    /// offset (e.g. 700 Hz) rather than an absolute RBN frequency. This is
    /// the manual equivalent of entering the dial frequency once -- manta
    /// does not poll the rig (no CAT; README non-goals).
    ///
    /// Sideband convention: `center_freq_hz()` is added to the decoded
    /// audio-tone offset as-is, so pass the suppressed-carrier/USB dial
    /// reading. On a CW-mode dial display, subtract your sidetone pitch
    /// first, or the reported frequency reads high by the pitch amount.
    ///
    /// Rejects non-finite and non-positive values, matching the CLI's
    /// `--dial-freq-hz` parser: "no reference" is expressed by not calling
    /// this, not by passing 0.0.
    pub fn with_center_freq_hz(mut self, center_freq_hz: f64) -> Result<Self> {
        if !center_freq_hz.is_finite() || center_freq_hz <= 0.0 {
            return Err(anyhow!(
                "center frequency must be a finite, positive number of Hz, \
                 got {center_freq_hz}"
            ));
        }
        self.center_freq_hz = center_freq_hz;
        Ok(self)
    }

    /// Open the named input device (default device if `None`). Requires the
    /// device's native rate to be exactly TARGET_RATE_HZ (48000) -- M1 does
    /// not resample; see AudioIqSource::new for the rate-mismatch error.
    ///
    /// The returned source is live: `read` waits for samples and fails
    /// after `DEVICE_STALL_TIMEOUT` of none (MAN-231).
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
        AudioIqSource::live(Box::new(cpal_src), DEVICE_STALL_TIMEOUT)
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
        self.center_freq_hz // 0.0 when no dial frequency was supplied -- MAN-34.
    }

    /// The rig's AF passband, ABOVE the dial frequency only -- never
    /// `+/- TARGET_RATE_HZ / 2`. See `AUDIO_PASSBAND_LO_HZ`.
    fn rf_passband_hz(&self) -> (f64, f64) {
        (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let got = self.read_real(&mut real)?;
        if got == 0 {
            return Ok(0);
        }
        let analytic = self.hilbert.process(&real[..got]);
        buf[..got].copy_from_slice(&analytic);
        Ok(got)
    }
}

impl AudioIqSource {
    /// Real samples from the underlying source. A file returns 0 at end of
    /// file; a live device polls its ring until at least one sample
    /// arrives, or fails after its stall timeout. An empty request never
    /// waits.
    fn read_real(&mut self, real: &mut [f32]) -> Result<usize> {
        let Some(live) = &self.live else {
            return self.src.read(real);
        };
        if real.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        loop {
            let got = self.src.read(real)?;
            if got > 0 {
                return Ok(got);
            }
            if started.elapsed() >= live.stall_timeout {
                bail!(
                    "audio input delivered no samples in {} s at {TARGET_RATE_HZ} Hz",
                    live.stall_timeout.as_secs_f64()
                );
            }
            std::thread::sleep(DEVICE_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A fake capture ring with the semantics of coppa's `CpalSource`: each
    /// read pops the next scripted sample count (0 = empty ring); an
    /// exhausted script reads as an empty ring forever, like a callback
    /// that never fires.
    struct ScriptedRing {
        script: VecDeque<usize>,
    }

    impl AudioSource for ScriptedRing {
        fn read(&mut self, buf: &mut [f32]) -> Result<usize> {
            let n = self.script.pop_front().unwrap_or(0).min(buf.len());
            buf[..n].fill(0.5);
            Ok(n)
        }
        fn sample_rate(&self) -> u32 {
            TARGET_RATE_HZ
        }
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn scripted(script: &[usize], stall: Duration) -> AudioIqSource {
        let ring = ScriptedRing {
            script: script.iter().copied().collect(),
        };
        AudioIqSource::live(Box::new(ring), stall).unwrap()
    }

    #[test]
    fn a_live_device_waits_out_an_empty_capture_ring() {
        let mut aiq = scripted(&[0, 0, 0, 480], Duration::from_secs(1));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 480);
    }

    #[test]
    fn a_live_device_that_never_delivers_fails_within_its_stall_timeout() {
        let stall = Duration::from_millis(50);
        let mut aiq = scripted(&[], stall);
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        let started = Instant::now();
        let err = aiq.read(&mut buf).unwrap_err().to_string();
        let elapsed = started.elapsed();
        assert!(err.contains("delivered no samples"), "{err}");
        assert_eq!(
            err,
            "audio input delivered no samples in 0.05 s at 48000 Hz"
        );
        assert!(
            elapsed >= stall && elapsed < Duration::from_secs(1),
            "elapsed={elapsed:?}"
        );
    }

    #[test]
    fn an_empty_request_does_not_block_a_live_device() {
        let mut aiq = scripted(&[], Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(aiq.read(&mut []).unwrap(), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

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
        // MAN-86 review: coverage is the rig's AF passband above the dial,
        // NOT the analytic stream's +/- 24 kHz Nyquist span.
        assert_eq!(
            aiq.rf_passband_hz(),
            (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
        );
        assert!(
            aiq.rf_passband_hz().0 > 0.0,
            "analytic audio carries no spectrum below the dial frequency"
        );
        assert!(aiq.rf_passband_hz().1 < aiq.sample_rate() / 2.0);
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
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 5],
            TARGET_RATE_HZ,
        ));
        let mut aiq = AudioIqSource::new(src).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(aiq.read(&mut buf).unwrap(), 5);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn center_freq_hz_defaults_to_zero_without_an_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        assert_eq!(AudioIqSource::new(src).unwrap().center_freq_hz(), 0.0);
    }

    #[test]
    fn with_center_freq_hz_is_reported_as_the_sources_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        let aiq = AudioIqSource::new(src)
            .unwrap()
            .with_center_freq_hz(14_030_000.0)
            .unwrap();
        assert_eq!(aiq.center_freq_hz(), 14_030_000.0);
    }

    #[test]
    fn with_center_freq_hz_rejects_non_finite_and_non_positive_values() {
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -14_030_000.0,
        ] {
            let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
                vec![0.0; 10],
                TARGET_RATE_HZ,
            ));
            assert!(
                AudioIqSource::new(src)
                    .unwrap()
                    .with_center_freq_hz(bad)
                    .is_err(),
                "with_center_freq_hz({bad}) should have been rejected"
            );
        }
    }
}
