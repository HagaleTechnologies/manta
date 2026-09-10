//! IQ sources. At M0: WAV file playback only (ARCHITECTURE §3).
//! WAV layout: 2 channels, ch0 = I, ch1 = Q; Float32 or Int16.
//! Center frequency comes from a JSON sidecar `<stem>.json`.

pub mod audio;
pub use audio::{AudioIqSource, TARGET_RATE_HZ};

pub mod kiwi;
pub use kiwi::KiwiIqSource;

#[cfg(feature = "soapy")]
pub mod soapy;
#[cfg(feature = "soapy")]
pub use soapy::SoapySdrIqSource;

#[cfg(feature = "hpsdr")]
pub mod hpsdr;
#[cfg(feature = "hpsdr")]
pub use hpsdr::{HpsdrConfig, HpsdrDevice, HpsdrIqSource};

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use std::path::Path;

/// Packet-level health counters for an input source that can lose or
/// discard whole packets on the wire (MAN-56, following MAN-22's
/// malformed-packet counting).
///
/// Defined at the crate root, deliberately NOT inside the
/// `#[cfg(feature = "hpsdr")]` `hpsdr` module: `IqSource` itself is
/// compiled unconditionally, so naming a feature-gated type in
/// `health_counters`' signature would make the trait -- and every
/// non-HPSDR build, including plain `cargo test --workspace` -- require
/// `--features hpsdr`.
///
/// Atomic rather than mutex-guarded for the same reason
/// `HpsdrIqSource::confirmed_live` is (MAN-55): a metrics reader must
/// never take the packet-pump's lock just to read counters. Reading all
/// three is not one atomic snapshot -- these are independent monotonic
/// counters sampled on a timer, where a straddled read is
/// indistinguishable from a sample taken a microsecond earlier.
#[derive(Debug, Default)]
pub struct InputHealthCounters {
    dropped_packets: std::sync::atomic::AtomicU64,
    gaps_detected: std::sync::atomic::AtomicU64,
    malformed_packets: std::sync::atomic::AtomicU64,
}

impl InputHealthCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// `n` packets are estimated to have been lost in transit.
    pub fn record_dropped(&self, n: u64) {
        self.dropped_packets
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// One gap *event* (which may account for several dropped packets).
    pub fn record_gap(&self) {
        self.gaps_detected
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// One datagram discarded as not-a-valid-packet (MAN-22).
    pub fn record_malformed(&self) {
        self.malformed_packets
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn dropped_packets(&self) -> u64 {
        self.dropped_packets
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn gaps_detected(&self) -> u64 {
        self.gaps_detected
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn malformed_packets(&self) -> u64 {
        self.malformed_packets
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A source of complex IQ samples: file, SDR, or (later) audio/network. ARCHITECTURE §3.
pub trait IqSource {
    /// The source's native complex sample rate, S/s.
    fn sample_rate(&self) -> f64;
    /// The source's RF center frequency, Hz (0.0 if unknown).
    fn center_freq_hz(&self) -> f64;
    /// Fill `buf`, returning the number of samples written; 0 = EOF.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize>;

    /// A shared liveness flag for sources where successfully *opening* a
    /// connection doesn't confirm a real, live device is actually present
    /// on the other end (MAN-55) -- e.g. HPSDR's UDP `connect`/initial
    /// `send` require no peer response at all, so `HpsdrDevice::open`
    /// succeeding proves nothing about whether anything is listening.
    /// Returns `None` (the default) for sources where opening already
    /// implies liveness -- KiwiSDR's WebSocket handshake, SoapySDR's
    /// hardware-open call, or a file both require a real response/handle
    /// to succeed at all. A caller with `Some(handle)` should treat the
    /// source as unconfirmed until `handle.load(Ordering::Relaxed)` first
    /// reads `true`, rather than assuming liveness the instant `open()`
    /// returns.
    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }

    /// Shared packet-loss/malformed counters for sources that can lose or
    /// discard whole packets on the wire (MAN-56). Returns `None` (the
    /// default) for sources with no such failure mode -- a file has no
    /// packets, and KiwiSDR/SoapySDR/audio currently count nothing of the
    /// kind. A caller with `Some(handle)` may keep polling it after the
    /// source itself has been moved into `manta_engine::listen`, which is
    /// the whole reason this is a shared handle rather than a `&self`
    /// snapshot getter.
    ///
    /// **Wrapper types must forward this**, exactly as `manta-cli`'s
    /// `FixedCenterFreqSource` does -- the default `None` otherwise
    /// silently swallows the inner source's counters. Not enforceable by
    /// the type system; see that impl.
    fn health_counters(&self) -> Option<std::sync::Arc<InputHealthCounters>> {
        None
    }
}

/// JSON sidecar alongside a WAV fixture, carrying metadata the WAV format itself can't. ARCHITECTURE §3.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Sidecar {
    pub center_freq_hz: f64,
}

/// Stereo WAV file (ch0=I, ch1=Q) as an IqSource, with an optional `<stem>.json` sidecar for center frequency. ARCHITECTURE §3.
pub struct WavIqSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl WavIqSource {
    /// Eager-loads the whole file (M0 pinned decision 15; files are <~100 MB). ARCHITECTURE §3.
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader =
            hound::WavReader::open(path).with_context(|| format!("open WAV {}", path.display()))?;
        let spec = reader.spec();
        if spec.channels != 2 {
            bail!("IQ WAV must have 2 channels (I, Q); got {}", spec.channels);
        }
        let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Float, 32) => {
                reader.samples::<f32>().collect::<Result<_, _>>()?
            }
            (hound::SampleFormat::Int, 16) => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.0))
                .collect::<Result<_, _>>()?,
            (f, b) => bail!("unsupported WAV format {f:?}/{b}-bit (need Float32 or Int16)"),
        };
        let samples = interleaved
            .chunks_exact(2)
            .map(|c| Complex32::new(c[0], c[1]))
            .collect();

        let sidecar_path = path.with_extension("json");
        let center_freq_hz = if sidecar_path.exists() {
            let text = std::fs::read_to_string(&sidecar_path)
                .with_context(|| format!("read sidecar {}", sidecar_path.display()))?;
            let sc: Sidecar = serde_json::from_str(&text)
                .with_context(|| format!("parse sidecar {}", sidecar_path.display()))?;
            sc.center_freq_hz
        } else {
            0.0
        };

        Ok(WavIqSource {
            samples,
            cursor: 0,
            fs: spec.sample_rate as f64,
            center_freq_hz,
        })
    }
}

impl IqSource for WavIqSource {
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

/// Drain an IqSource to a Vec (file-mode helper). ARCHITECTURE §3.
pub fn read_all(src: &mut dyn IqSource) -> Result<Vec<Complex32>> {
    let mut all = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); 65_536];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(all);
        }
        all.extend_from_slice(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;
    use std::io::Write;

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

    fn samples() -> Vec<Complex32> {
        (0..1000)
            .map(|i| Complex32::new(i as f32 / 1000.0, -(i as f32) / 2000.0))
            .collect()
    }

    #[test]
    fn reads_f32_wav_with_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let mut f = std::fs::File::create(dir.path().join("fix.json")).unwrap();
        f.write_all(br#"{"center_freq_hz": 14000000.0}"#).unwrap();

        let mut src = WavIqSource::open(&wav).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let all = read_all(&mut src).unwrap();
        assert_eq!(all, samples());
    }

    #[test]
    fn missing_sidecar_means_zero_center() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let src = WavIqSource::open(&wav).unwrap();
        assert_eq!(src.center_freq_hz(), 0.0);
    }

    #[test]
    fn reads_i16_wav_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix16.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 96_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        w.write_sample(16384i16).unwrap(); // I = 0.5
        w.write_sample(-16384i16).unwrap(); // Q = -0.5
        w.finalize().unwrap();
        let mut src = WavIqSource::open(&wav).unwrap();
        let all = read_all(&mut src).unwrap();
        assert_eq!(all.len(), 1);
        assert!((all[0].re - 0.5).abs() < 1e-4);
        assert!((all[0].im + 0.5).abs() < 1e-4);
    }

    #[test]
    fn mono_wav_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("mono.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 96_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        w.write_sample(0.0f32).unwrap();
        w.finalize().unwrap();
        assert!(WavIqSource::open(&wav).is_err());
    }

    // MAN-56: input-layer health counters.

    #[test]
    fn a_source_with_no_packet_loss_model_reports_no_health_counters() {
        // The trait's default: only sources that actually count something
        // override it (MAN-56 D1), so file/audio/kiwi/soapy stay untouched.
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let src = WavIqSource::open(&wav).unwrap();
        assert!(src.health_counters().is_none());
    }

    #[test]
    fn input_health_counters_accumulate_monotonically() {
        let c = InputHealthCounters::new();
        assert_eq!(
            (
                c.dropped_packets(),
                c.gaps_detected(),
                c.malformed_packets()
            ),
            (0, 0, 0)
        );
        c.record_dropped(3);
        c.record_dropped(2);
        c.record_gap();
        c.record_malformed();
        c.record_malformed();
        assert_eq!(
            (
                c.dropped_packets(),
                c.gaps_detected(),
                c.malformed_packets()
            ),
            (5, 1, 2)
        );
    }

    #[test]
    fn read_respects_buffer_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let mut src = WavIqSource::open(&wav).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 300];
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 100);
        assert_eq!(src.read(&mut buf).unwrap(), 0); // EOF
    }
}
