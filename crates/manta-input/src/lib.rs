//! IQ sources. At M0: WAV file playback only (ARCHITECTURE §3).
//! WAV layout: 2 channels, ch0 = I, ch1 = Q; Float32 or 16/24/32-bit integer PCM, streamed.
//! Center frequency comes from a JSON sidecar `<stem>.json`.

pub mod devices;

pub mod audio;
pub use audio::{AudioIqSource, AUDIO_PASSBAND_HI_HZ, AUDIO_PASSBAND_LO_HZ, TARGET_RATE_HZ};

pub mod kiwi;
pub use kiwi::KiwiIqSource;

pub mod ka9q;
pub use ka9q::{Ka9qConfig, Ka9qIqSource};

pub mod decimate;
pub use decimate::DecimatingSource;

pub mod replay;
pub use replay::{LoopingSource, PacedSource, Reopen};

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
use std::path::{Path, PathBuf};

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

    /// The RF passband this source actually delivers, as `(lo, hi)`
    /// offsets in Hz from `center_freq_hz()` -- which is NOT always
    /// `-sample_rate()/2 .. +sample_rate()/2`, and is NOT always
    /// symmetric about the centre.
    ///
    /// MAN-86 review: a source that resamples reports a *processing* rate
    /// wider than the spectrum it carries, and a source fed from a rig's
    /// audio output carries spectrum only on ONE side of its dial
    /// frequency. `KiwiIqSource` upsamples a ~12 kS/s receiver stream to
    /// 96 kS/s while the receiver itself is configured for a 10 kHz IQ
    /// passband; `AudioIqSource` Hilbert-transforms a rig's ~3 kHz audio
    /// output, whose tones are positive offsets above `--dial-freq-hz`.
    /// Reading half the sample rate as the decodable half-width would
    /// make `SKIMMER/SETT` advertise centre +/-48 kHz / dial +/-24 kHz of
    /// coverage to Aggregator that no signal ever occupies. Anything a
    /// consumer publishes as *coverage* (SETT segments) must use this;
    /// anything that is a per-sample timing quantity (the channelizer,
    /// `SpotBus`'s sample-index-to-wall-clock conversion) must keep using
    /// `sample_rate()`.
    ///
    /// Defaults to the full Nyquist span, correct for every source that
    /// neither resamples nor works from real audio (file, SoapySDR,
    /// HPSDR), where the delivered spectrum IS the Nyquist span of the
    /// stream.
    fn rf_passband_hz(&self) -> (f64, f64) {
        let half = self.sample_rate() / 2.0;
        (-half, half)
    }

    /// MAN-196: the band inside which this source's own filtering keeps
    /// channels within 1 dB of flat, as `(lo, hi)` Hz relative to the
    /// centre. `None` (the default) means every channel is trusted --
    /// today's behaviour for every non-decimated source, whose delivered
    /// spectrum needs no additional boundary beyond `rf_passband_hz`.
    /// `DecimatingSource` overrides this from its decimator's
    /// `trusted_half_bandwidth_hz()`; consumers advertise it (the usable
    /// passband) and may veto track spawns beyond it. A source whose
    /// trusted band is narrower than its `rf_passband_hz` clips the
    /// passband it reports to the trusted band, so the two never
    /// disagree.
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        None
    }
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

    /// MAN-73: number of samples (at `sample_rate()`) the source *missed*
    /// immediately before the samples returned by the most recent `read()`
    /// -- e.g. a live connection that was lost and re-established. Returns
    /// the value once, then `None` until the next gap. Default `None`:
    /// file and continuously-streaming sources never have gaps.
    /// `manta_engine::listen` responds by closing the current track
    /// segment and starting a fresh one whose sample clock is advanced by
    /// this many samples, so spot timestamps stay wall-clock-true and no
    /// audio is spliced across the outage. Deliberately NOT zero-fill: a
    /// zero-filled outage past ~2.5s pins `manta-dsp::floor`'s 25th-
    /// percentile noise floor at -140 dBFS and floods false tracks on
    /// resume (measured; see docs/DECISIONS/2026-10-05-man73-source-reconnect.md).
    fn take_discontinuity(&mut self) -> Option<u64> {
        None
    }

    /// Shared packet-loss/malformed counters for sources that can lose or
    /// discard whole packets on the wire (MAN-56). Returns `None` (the
    /// default) for sources with no such failure mode -- a file has no
    /// packets. KiwiSDR counts SND `seq` gaps and short frames (MAN-128);
    /// SoapySDR/audio currently count nothing of the kind. A caller with
    /// `Some(handle)` may keep polling it after the
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

/// Where `wav`'s sidecar lives: `<stem>.json` next to it. ARCHITECTURE §3.
pub fn sidecar_path(wav: &Path) -> PathBuf {
    wav.with_extension("json")
}

/// The `<stem>.json` sidecar of `wav`, if one exists (ARCHITECTURE §3).
/// `None` when there is no such file; malformed JSON is an error naming it.
pub fn read_sidecar(wav: &Path) -> Result<Option<Sidecar>> {
    let sidecar_path = sidecar_path(wav);
    if !sidecar_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&sidecar_path)
        .with_context(|| format!("read sidecar {}", sidecar_path.display()))?;
    let sc: Sidecar = serde_json::from_str(&text)
        .with_context(|| format!("parse sidecar {}", sidecar_path.display()))?;
    Ok(Some(sc))
}

/// The highest sample rate an IQ WAV may declare, in Hz (MAN-269).
///
/// `WavIqSource::open` checks only the size here, not the shape: whether
/// `fs / 93.75` is a power of two stays `Channelizer::new`'s job, with its
/// own error message. The size check exists because a shape-valid but absurd
/// rate (393 216 000 Hz = 93.75 * 2^22, in a 76-byte header) passes the
/// channelizer and then asks `FloorBank::new` for 3.36 GB, before the
/// calibration buffer is even sized. 10 MS/s matches `manta-cli`'s
/// `MAX_HPSDR_RATE_HZ`; at the largest table rate it admits (6.144 MS/s) a
/// replay peaked at 163 MiB RSS. See
/// docs/DECISIONS/2026-10-10-man269-paced-looping-replay.md.
pub const MAX_IQ_WAV_RATE_HZ: f64 = 10_000_000.0;

type WavFile = std::io::BufReader<std::fs::File>;

/// Stereo WAV file (ch0=I, ch1=Q) as an IqSource, with an optional `<stem>.json` sidecar for center frequency. ARCHITECTURE §3.
/// Streams: `open` reads the header only and `read` decodes just the frames it
/// returns, so memory does not grow with the file (MAN-119).
pub struct WavIqSource {
    frames: WavFrames,
    path: PathBuf,
    fs: f64,
    center_freq_hz: f64,
}

/// The data chunk, as the sample type hound decodes it to.
enum WavFrames {
    Float(hound::WavIntoSamples<WavFile, f32>),
    /// 16-, 24- or 32-bit PCM through `i32`, divided by `full_scale` = 2^(bits-1).
    Int {
        samples: hound::WavIntoSamples<WavFile, i32>,
        full_scale: f32,
    },
}

impl WavIqSource {
    /// Opens `path`, reading only its header. ARCHITECTURE §3.
    /// The centre frequency comes from the `<stem>.json` sidecar, `0.0` without one.
    pub fn open(path: &Path) -> Result<Self> {
        let mut src = Self::open_with_center_freq_hz(path, 0.0)?;
        src.center_freq_hz = read_sidecar(path)?.map_or(0.0, |sc| sc.center_freq_hz);
        Ok(src)
    }

    /// `open` with a caller-supplied centre frequency; the sidecar is not
    /// read at all, so a malformed or foreign `<stem>.json` cannot fail it
    /// (MAN-131, `decode --center-freq-hz`).
    pub fn open_with_center_freq_hz(path: &Path, center_freq_hz: f64) -> Result<Self> {
        let reader =
            hound::WavReader::open(path).with_context(|| format!("open WAV {}", path.display()))?;
        let spec = reader.spec();
        if spec.channels != 2 {
            bail!("IQ WAV must have 2 channels (I, Q); got {}", spec.channels);
        }
        // Before any sample is read: every rate-sized allocation downstream
        // trusts this number (MAN-269, MAX_IQ_WAV_RATE_HZ).
        let fs = f64::from(spec.sample_rate);
        if fs <= 0.0 || fs > MAX_IQ_WAV_RATE_HZ {
            bail!(
                "IQ WAV {} declares a sample rate of {} Hz; IQ replay needs a rate above 0 \
                 and at most {} Hz",
                path.display(),
                spec.sample_rate,
                MAX_IQ_WAV_RATE_HZ as u64
            );
        }
        let frames = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Float, 32) => WavFrames::Float(reader.into_samples()),
            (hound::SampleFormat::Int, bits @ (16 | 24 | 32)) => WavFrames::Int {
                samples: reader.into_samples(),
                full_scale: (1u64 << (bits - 1)) as f32,
            },
            (f, b) => return Err(unsupported_wav(f, b)),
        };

        Ok(WavIqSource {
            frames,
            path: path.to_path_buf(),
            fs,
            center_freq_hz,
        })
    }

    /// The next (I, Q) frame; `None` at EOF. hound rejects a data chunk
    /// holding a half frame at `open`, so a lone trailing I sample is only
    /// handled defensively (dropped, as `as_chunks::<2>` did).
    fn next_frame(&mut self) -> Result<Option<Complex32>> {
        let (re, im) = match &mut self.frames {
            WavFrames::Float(samples) => (samples.next(), samples.next()),
            WavFrames::Int {
                samples,
                full_scale,
            } => {
                let full_scale = *full_scale;
                let mut next = || samples.next().map(|r| r.map(|v| v as f32 / full_scale));
                (next(), next())
            }
        };
        let path = &self.path;
        let context = || format!("read WAV {}", path.display());
        match (re, im) {
            (Some(re), Some(im)) => Ok(Some(Complex32::new(
                re.with_context(context)?,
                im.with_context(context)?,
            ))),
            (Some(Err(e)), None) => Err(e).with_context(context),
            _ => Ok(None),
        }
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
        let mut n = 0;
        while n < buf.len() {
            match self.next_frame()? {
                Some(frame) => buf[n] = frame,
                None => break,
            }
            n += 1;
        }
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

fn unsupported_wav(format: hound::SampleFormat, bits: u16) -> anyhow::Error {
    let name = match format {
        hound::SampleFormat::Float => "Float",
        hound::SampleFormat::Int => "Int",
    };
    anyhow::anyhow!("unsupported WAV format {name}/{bits}-bit (need Float32 or Int16/24/32)")
}

#[cfg(test)]
mod tests {

    #[test]
    fn unsupported_wav_text_names_sample_formats() {
        assert_eq!(
            unsupported_wav(hound::SampleFormat::Float, 64).to_string(),
            "unsupported WAV format Float/64-bit (need Float32 or Int16/24/32)"
        );
        assert_eq!(
            unsupported_wav(hound::SampleFormat::Int, 8).to_string(),
            "unsupported WAV format Int/8-bit (need Float32 or Int16/24/32)"
        );
    }

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
    fn read_sidecar_is_none_without_a_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        assert!(read_sidecar(&wav).unwrap().is_none());
    }

    #[test]
    fn read_sidecar_returns_the_center_frequency() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        std::fs::write(
            dir.path().join("fix.json"),
            br#"{"center_freq_hz": 7030000.0}"#,
        )
        .unwrap();
        assert_eq!(sidecar_path(&wav), dir.path().join("fix.json"));
        let sc = read_sidecar(&wav).unwrap().expect("sidecar present");
        assert_eq!(sc.center_freq_hz, 7_030_000.0);
    }

    #[test]
    fn read_sidecar_rejects_malformed_json_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let json = dir.path().join("fix.json");
        std::fs::write(&json, b"{not json").unwrap();
        let err = format!("{:#}", read_sidecar(&wav).unwrap_err());
        let want = format!("parse sidecar {}", json.display());
        assert!(err.contains(&want), "{err}");
        // `WavIqSource::open` reports the same text.
        let open_err = format!("{:#}", WavIqSource::open(&wav).err().expect("bad sidecar"));
        assert!(open_err.contains(&want), "{open_err}");
    }

    #[test]
    fn open_with_center_freq_hz_never_reads_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        std::fs::write(dir.path().join("fix.json"), b"{not json").unwrap();
        let src = WavIqSource::open_with_center_freq_hz(&wav, 7_030_000.0).unwrap();
        assert_eq!(src.center_freq_hz(), 7_030_000.0);
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

    /// The 24-bit test frames: two (I, Q) pairs and what they read back as.
    const I24_SAMPLES: [i32; 4] = [4_194_304, -4_194_304, 8_388_607, -8_388_608];

    fn i24_frames() -> Vec<Complex32> {
        vec![
            Complex32::new(0.5, -0.5),
            Complex32::new(8_388_607.0 / 8_388_608.0, -1.0),
        ]
    }

    fn int_spec(bits_per_sample: u16) -> hound::WavSpec {
        hound::WavSpec {
            channels: 2,
            sample_rate: 96_000,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        }
    }

    /// A RIFF/WAVE file from a raw `fmt ` chunk body and `data` bytes, for
    /// header shapes `hound::WavWriter` does not write.
    fn riff_wav(path: &std::path::Path, fmt: &[u8], data: &[u8]) {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        b.extend_from_slice(fmt);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        std::fs::write(path, b).unwrap();
    }

    /// A 16-byte `fmt ` body: 2 channels at 96 kHz.
    fn fmt_body(tag: u16, bytes_per_sample: u16, bits: u16) -> Vec<u8> {
        let block_align = 2 * bytes_per_sample;
        let mut f = Vec::new();
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&96_000u32.to_le_bytes());
        f.extend_from_slice(&(96_000 * u32::from(block_align)).to_le_bytes());
        f.extend_from_slice(&block_align.to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f
    }

    #[test]
    fn reads_i24_wav_normalized() {
        // hound writes this as WAVE_FORMAT_EXTENSIBLE.
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix24.wav");
        let mut w = hound::WavWriter::create(&wav, int_spec(24)).unwrap();
        for v in I24_SAMPLES {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let all = read_all(&mut WavIqSource::open(&wav).unwrap()).unwrap();
        assert_eq!(all, i24_frames());
    }

    #[test]
    fn reads_pcm24_wav_without_an_extensible_header() {
        // Format tag 1 (plain PCM) with 24 bits: the header shape CWSL and
        // Python's `wave` write.
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("pcm24.wav");
        let data: Vec<u8> = I24_SAMPLES
            .iter()
            .flat_map(|v| v.to_le_bytes()[..3].to_vec())
            .collect();
        riff_wav(&wav, &fmt_body(1, 3, 24), &data);
        let all = read_all(&mut WavIqSource::open(&wav).unwrap()).unwrap();
        assert_eq!(all, i24_frames());
    }

    #[test]
    fn reads_24_bit_samples_in_4_byte_containers() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix24in32.wav");
        let spec_ex = hound::WavSpecEx {
            spec: int_spec(24),
            bytes_per_sample: 4,
        };
        let file = std::io::BufWriter::new(std::fs::File::create(&wav).unwrap());
        let mut w = hound::WavWriter::new_with_spec_ex(file, spec_ex).unwrap();
        for v in I24_SAMPLES {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let all = read_all(&mut WavIqSource::open(&wav).unwrap()).unwrap();
        assert_eq!(all, i24_frames());
    }

    #[test]
    fn reads_i32_wav_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix32.wav");
        let mut w = hound::WavWriter::create(&wav, int_spec(32)).unwrap();
        w.write_sample(1_073_741_824i32).unwrap(); // I = 0.5
        w.write_sample(i32::MIN).unwrap(); // Q = -1.0
        w.finalize().unwrap();
        let all = read_all(&mut WavIqSource::open(&wav).unwrap()).unwrap();
        assert_eq!(all, vec![Complex32::new(0.5, -1.0)]);
    }

    #[test]
    fn open_reads_only_the_header() {
        // The header declares 1000 Float32 frames; the file holds 10. An
        // eager open fails here ("Failed to read enough bytes." from `open`);
        // a streaming open succeeds, returns the 10 frames, then errors
        // naming the file.
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("trunc.wav");
        header_only_iq_wav(&wav, 96_000, 1000);
        let mut bytes = std::fs::read(&wav).unwrap();
        bytes.truncate(44);
        for i in 0..10u8 {
            bytes.extend_from_slice(&(f32::from(i) / 10.0).to_le_bytes());
            bytes.extend_from_slice(&0.0f32.to_le_bytes());
        }
        std::fs::write(&wav, bytes).unwrap();

        let mut src = WavIqSource::open_with_center_freq_hz(&wav, 14_000_000.0).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 10];
        assert_eq!(src.read(&mut buf).unwrap(), 10);
        assert_eq!(buf[3], Complex32::new(0.3, 0.0));
        let err = format!("{:#}", src.read(&mut buf).unwrap_err());
        assert!(
            err.contains(&format!("read WAV {}", wav.display())),
            "{err}"
        );
    }

    #[test]
    fn a_trailing_half_frame_is_rejected_at_open() {
        // A data chunk of 3 Float32 samples: hound 3.5.1 checks the sample
        // count against the channel count in the header, so the half frame
        // never reaches `read` (as on HEAD, where `open` failed the same way).
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("half.wav");
        let data: Vec<u8> = [0.25f32, -0.25, 0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        riff_wav(&wav, &fmt_body(3, 4, 32), &data);
        let err = format!("{:#}", WavIqSource::open(&wav).err().expect("half frame"));
        assert!(
            err.contains(&format!("open WAV {}", wav.display())),
            "{err}"
        );
        assert!(err.contains("invalid data chunk length"), "{err}");
    }

    #[test]
    fn eight_bit_and_float64_wavs_are_rejected_naming_the_formats() {
        let dir = tempfile::tempdir().unwrap();
        let wav8 = dir.path().join("fix8.wav");
        let mut w = hound::WavWriter::create(&wav8, int_spec(8)).unwrap();
        w.write_sample(64i8).unwrap();
        w.write_sample(-64i8).unwrap();
        w.finalize().unwrap();
        let err = WavIqSource::open(&wav8).err().expect("8-bit").to_string();
        assert_eq!(
            err,
            "unsupported WAV format Int/8-bit (need Float32 or Int16/24/32)"
        );

        // hound reads Float64 only from a WAVE_FORMAT_EXTENSIBLE header (a
        // plain tag-3 header fails inside hound as "bits per sample is not 32").
        let wav64 = dir.path().join("fix64.wav");
        let mut fmt = fmt_body(0xFFFE, 8, 64);
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&64u16.to_le_bytes()); // wValidBitsPerSample
        fmt.extend_from_slice(&3u32.to_le_bytes()); // dwChannelMask
        fmt.extend_from_slice(&[
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
            0x9b, 0x71,
        ]); // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        riff_wav(&wav64, &fmt, &[0u8; 16]);
        let err = WavIqSource::open(&wav64)
            .err()
            .expect("Float64")
            .to_string();
        assert_eq!(
            err,
            "unsupported WAV format Float/64-bit (need Float32 or Int16/24/32)"
        );
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

    /// A float32 stereo WAV header declaring `rate` and `frames` frames of
    /// silence, written byte by byte so the test never depends on
    /// `hound::WavWriter` accepting an absurd rate.
    fn header_only_iq_wav(path: &std::path::Path, rate: u32, frames: u32) {
        let data_len = frames * 8;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        b.extend_from_slice(&2u16.to_le_bytes()); // I, Q
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&rate.wrapping_mul(8).to_le_bytes()); // byte rate
        b.extend_from_slice(&8u16.to_le_bytes()); // block align
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        b.resize(b.len() + data_len as usize, 0);
        std::fs::write(path, b).unwrap();
    }

    fn open_at(rate: u32) -> Result<WavIqSource> {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("rate.wav");
        header_only_iq_wav(&wav, rate, 4);
        WavIqSource::open(&wav)
    }

    // MAN-269: the two PR #135 findings -- validate/cap the replay rate
    // before anything is allocated from it.

    #[test]
    fn iq_wav_declaring_a_rate_above_the_replay_ceiling_is_rejected() {
        // 393_216_000 = 93.75 * 2^22: shape-valid for the channelizer, so only
        // a magnitude bound stops it (measured: FloorBank::new asked for
        // 3.36 GB).
        let err = open_at(393_216_000).err().unwrap().to_string();
        assert!(err.contains("393216000 Hz"), "{err}");
        assert!(
            err.contains("IQ replay needs a rate above 0 and at most 10000000 Hz"),
            "{err}"
        );
    }

    #[test]
    fn iq_wav_rate_ceiling_is_inclusive() {
        let src = open_at(10_000_000).unwrap();
        assert_eq!(src.sample_rate(), 10_000_000.0);
        let err = open_at(10_000_001).err().unwrap().to_string();
        assert!(err.contains("10000001 Hz"), "{err}");
    }

    #[test]
    fn iq_wav_at_the_largest_admitted_table_rate_opens() {
        // 6_144_000 = 93.75 * 2^16, the largest channelizer table rate under
        // the ceiling.
        let mut src = open_at(6_144_000).unwrap();
        assert_eq!(src.sample_rate(), 6_144_000.0);
        assert_eq!(read_all(&mut src).unwrap().len(), 4);
    }

    #[test]
    fn iq_wav_declaring_zero_hz_is_rejected() {
        let err = open_at(0).err().unwrap().to_string();
        assert!(err.contains("a sample rate of 0 Hz"), "{err}");
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
    fn default_take_discontinuity_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let mut src = WavIqSource::open(&wav).unwrap();
        assert_eq!(src.take_discontinuity(), None);
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
