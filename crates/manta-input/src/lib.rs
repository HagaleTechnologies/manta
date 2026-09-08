//! IQ sources. At M0: WAV file playback only (ARCHITECTURE §3).
//! WAV layout: 2 channels, ch0 = I, ch1 = Q; Float32 or Int16.
//! Center frequency comes from a JSON sidecar `<stem>.json`.

pub mod audio;
pub use audio::{AudioIqSource, TARGET_RATE_HZ};

pub mod kiwi;
pub use kiwi::KiwiIqSource;

pub mod pace;
pub use pace::PacedSource;

pub mod replay;
pub use replay::LoopingWavSource;

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

/// Open a replay WAV as whichever `IqSource` its layout implies: 2 channels
/// = complex IQ (what `manta gen` writes and `manta decode` reads, at its
/// own native rate, via `WavIqSource`); anything else = a real rig-audio
/// passband, which `AudioIqSource` converts to analytic form via Hilbert
/// transform and still requires at exactly `TARGET_RATE_HZ`.
///
/// `manta_engine::listen()` is rate-agnostic -- `Channelizer::new` accepts
/// any `fs` where `fs / 93.75` is a power of two, and 96000 / 93.75 = 1024
/// -- so this is a reader choice, not a resample. See MAN-121.
///
/// Channel count alone is only unambiguous away from `TARGET_RATE_HZ`
/// (48 kHz): `AudioIqSource` never accepted anything but 48 kHz, so a
/// 2-channel file at any other rate could never have been a rig-audio
/// capture before this dispatch existed, and routing it to `WavIqSource`
/// regresses nothing. At exactly 48 kHz a 2-channel file is genuinely
/// ambiguous -- it's both a legal `WavIqSource` rate and `AudioIqSource`'s
/// only rate, and a stereo soundcard recording of a receiver's passband
/// (a real, common rig-audio capture) is indistinguishable from IQ by
/// channel count alone. A `<stem>.json` sidecar (what `gen`/`decode`'s IQ
/// files always carry) breaks the tie in favor of IQ; with no sidecar, a
/// 48 kHz 2-channel file keeps its pre-MAN-121 `AudioIqSource` downmix
/// path rather than being silently misread as `Complex32::new(I, Q)`.
///
/// The IQ branch additionally rejects any rate the channelizer can't
/// accept (`fs/93.75` not a power of two), because it is the branch that
/// adopts the FILE's own rate -- see the guard's own comment for why that
/// has to happen here rather than in `manta_engine::listen`.
///
/// The tie-break asks `replay_wav_has_iq_sidecar` -- "is there a parseable
/// IQ sidecar?" -- and deliberately NOT `replay_wav_center_freq_hz`, which
/// answers the separate question "does that sidecar declare an RF
/// frequency good enough to satisfy the CLI's `--dial-freq-hz` gate?"
/// (round-2 review). Those are different questions: a baseband or
/// unknown-dial capture whose sidecar says `center_freq_hz: 0.0` is still
/// unambiguously IQ in FORMAT, and conflating the two silently downmixed
/// such a 48 kHz file through `AudioIqSource`'s Hilbert path, discarding
/// Q, even when the caller supplied `--dial-freq-hz`.
pub fn open_replay_wav(path: &Path) -> Result<Box<dyn IqSource>> {
    let spec = hound::WavReader::open(path)
        .with_context(|| format!("open WAV {}", path.display()))?
        .spec();
    let is_iq = spec.channels == 2
        && (spec.sample_rate != TARGET_RATE_HZ || replay_wav_has_iq_sidecar(path));
    if is_iq {
        // The IQ path is the one that adopts the FILE's own rate, so it is
        // also the one that must bound it. `manta_engine::listen` sizes a
        // `CALIBRATION_SECONDS * fs` complex buffer from `sample_rate()`
        // and only THEN calls `Channelizer::new` to validate the rate, so
        // a malformed 2-channel header claiming a rate near `u32::MAX`
        // would ask for tens of GiB and abort the process instead of
        // reporting the documented unsupported-rate error. Reject it here,
        // before the source is ever handed to the engine (MAN-121 review).
        // `AudioIqSource` needs no equivalent guard -- it already accepts
        // exactly `TARGET_RATE_HZ` and nothing else.
        let fs = spec.sample_rate as f64;
        if !manta_dsp::channelizer::Channelizer::supports_rate(fs) {
            bail!(
                "unsupported sample rate {fs} in {}: an IQ replay WAV must use a channelizer \
                 rate (fs/93.75 a power of two -- 12/24/48/96/192/384 kHz and so on)",
                path.display()
            );
        }
        Ok(Box::new(WavIqSource::open(path)?))
    } else {
        Ok(Box::new(AudioIqSource::from_wav_file(path)?))
    }
}

/// Whatever `center_freq_hz` a replay WAV's sidecar declares: `Some` for 2
/// channels plus a parseable `<stem>.json` carrying a finite
/// `center_freq_hz` -- including `0.0`, which means "baseband/unknown", not
/// "not IQ" -- and `None` for anything else.
///
/// Deliberately swallows every error, including a nonexistent path, so a
/// CLI flag-validation gate can run BEFORE any real file I/O and still
/// report a missing flag rather than a missing file -- an ordering
/// `crates/manta-cli/tests/cli.rs`'s
/// `server_config_without_dial_freq_for_audio_source_is_a_clean_error`
/// asserts and documents in its own comment. Both public wrappers below
/// inherit that property.
fn replay_wav_sidecar_center_freq_hz(path: &Path) -> Option<f64> {
    let spec = hound::WavReader::open(path).ok()?.spec();
    if spec.channels != 2 {
        return None;
    }
    let text = std::fs::read_to_string(path.with_extension("json")).ok()?;
    let sc: Sidecar = serde_json::from_str(&text).ok()?;
    sc.center_freq_hz.is_finite().then_some(sc.center_freq_hz)
}

/// FORMAT question: does this replay WAV carry a valid IQ sidecar (2
/// channels plus a parseable `<stem>.json` with a finite `center_freq_hz`)?
///
/// This is the 48 kHz tie-break `open_replay_wav` uses, and it is
/// deliberately independent of what that frequency actually *is*: a
/// sidecar declaring `center_freq_hz: 0.0` (baseband, or a dial frequency
/// the operator will supply with `--dial-freq-hz` instead) still marks the
/// file as IQ, so its Q channel is read rather than thrown away.
pub fn replay_wav_has_iq_sidecar(path: &Path) -> bool {
    replay_wav_sidecar_center_freq_hz(path).is_some()
}

/// RF question: the real RF center frequency a replay WAV *declares* -- a
/// valid IQ sidecar whose `center_freq_hz` is finite and positive -- or
/// `None` for anything else, including an IQ file that declares `0.0`.
///
/// Only this answers "may `--server-config` go without `--dial-freq-hz`?";
/// use `replay_wav_has_iq_sidecar` to decide how to READ the file.
pub fn replay_wav_center_freq_hz(path: &Path) -> Option<f64> {
    replay_wav_sidecar_center_freq_hz(path).filter(|hz| *hz > 0.0)
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

    fn write_mono_f32_wav(path: &std::path::Path, samples: &[f32], fs: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: fs,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(*s).unwrap();
        }
        w.finalize().unwrap();
    }

    // MAN-121: `open_replay_wav` dispatches on channel count so `listen
    // --source` can accept the same 2-channel IQ WAV `manta gen`/`decode`
    // already use, not just AudioIqSource's mono rig-audio format.
    #[test]
    fn open_replay_wav_reads_a_stereo_iq_file_at_its_native_rate() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("v1.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        std::fs::write(
            dir.path().join("v1.json"),
            r#"{"center_freq_hz": 14000000.0}"#,
        )
        .unwrap();

        let src = open_replay_wav(&wav).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
    }

    #[test]
    fn open_replay_wav_reads_a_mono_48k_file_as_an_audio_source() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("rig.wav");
        write_mono_f32_wav(&wav, &vec![0.0f32; 480], 48_000);

        let src = open_replay_wav(&wav).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
        assert_eq!(src.center_freq_hz(), 0.0);
    }

    // MAN-121 remediation: a 2-channel 48 kHz WAV with no sidecar is the
    // exact collision with AudioIqSource's only supported rate -- a stereo
    // soundcard recording of rig audio looks identical to IQ by channel
    // count alone. Without a sidecar it must keep the pre-MAN-121
    // AudioIqSource downmix-and-Hilbert path, not be reinterpreted as
    // Complex32::new(I, Q). Ch0 is constant zero and ch1 carries a large,
    // distinctive value: WavIqSource would read the pair verbatim
    // (0.0, 0.9); AudioIqSource downmixes to ch0 alone (constant zero) and
    // the Hilbert transform of an all-zero signal is all-zero, so the two
    // paths are unambiguous from the output alone.
    #[test]
    fn open_replay_wav_treats_a_sidecarless_48k_stereo_file_as_rig_audio_not_iq() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("rig.wav");
        let samples: Vec<Complex32> = (0..300).map(|_| Complex32::new(0.0, 0.9)).collect();
        write_f32_wav(&wav, &samples, 48_000);

        let mut src = open_replay_wav(&wav).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
        assert_eq!(src.center_freq_hz(), 0.0);
        let all = read_all(&mut *src).unwrap();
        assert!(
            all.iter().all(|s| s.re == 0.0 && s.im == 0.0),
            "expected the AudioIqSource downmix+Hilbert path (all-zero output for \
             all-zero ch0), got non-zero samples -- the file was read as raw IQ instead"
        );
    }

    // Companion to the above: a sidecar is exactly the signal that should
    // still win at 48 kHz, since `gen`/`decode`'s own IQ files may legally
    // be 48 kHz (48000 / 93.75 = 512, a valid channelizer rate).
    #[test]
    fn open_replay_wav_still_reads_a_sidecar_backed_48k_stereo_file_as_iq() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("v1.wav");
        write_f32_wav(&wav, &samples(), 48_000);
        std::fs::write(
            dir.path().join("v1.json"),
            r#"{"center_freq_hz": 14000000.0}"#,
        )
        .unwrap();

        let mut src = open_replay_wav(&wav).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let all = read_all(&mut *src).unwrap();
        assert_eq!(all, samples());
    }

    // MAN-121 review: an unsupported IQ rate must be rejected HERE, not
    // deep inside `manta_engine::listen`, which sizes a two-second complex
    // calibration buffer from `sample_rate()` before `Channelizer::new`
    // ever validates it -- a header claiming a rate near `u32::MAX` would
    // request tens of GiB and abort the process instead of reporting this
    // error. 100 kHz is the smallest realistic case of the same class:
    // 100000 / 93.75 = 1066.67, not a power of two.
    #[test]
    fn open_replay_wav_rejects_an_iq_file_at_an_unsupported_channelizer_rate() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("odd.wav");
        write_f32_wav(&wav, &samples(), 100_000);

        // `Box<dyn IqSource>` isn't `Debug`, so `unwrap_err` is unavailable.
        let err = match open_replay_wav(&wav) {
            Ok(_) => panic!("expected an unsupported-rate error, got a source"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("unsupported sample rate"),
            "expected an unsupported-rate error, got: {err}"
        );
    }

    // The pathological case the guard above exists for, stated exactly: a
    // 2-channel header claiming 200 MS/s. `listen` would have asked for
    // 2 * 2e8 complex samples (~3.2 GiB) before validating the rate. (Not
    // literally `u32::MAX`, which `hound`'s own writer can't even encode
    // -- its byte-rate field overflows -- but the same class of header.)
    #[test]
    fn open_replay_wav_rejects_an_absurd_iq_sample_rate_before_any_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("huge.wav");
        write_f32_wav(&wav, &samples(), 200_000_000);

        assert!(open_replay_wav(&wav).is_err());
    }

    // Round-2 review: format detection and RF-frequency validation are
    // separate questions. A 48 kHz 2-channel IQ file whose sidecar declares
    // `center_freq_hz: 0.0` (baseband, or a dial frequency the operator
    // passes with --dial-freq-hz) is still IQ in FORMAT -- routing it to
    // AudioIqSource would discard Q and synthesize it back via Hilbert.
    // Same ch0=0 / ch1=0.9 discriminator as the sidecarless test above:
    // WavIqSource returns the pair verbatim, AudioIqSource returns zeros.
    #[test]
    fn open_replay_wav_reads_a_zero_freq_sidecar_48k_stereo_file_as_iq() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("baseband.wav");
        let samples: Vec<Complex32> = (0..300).map(|_| Complex32::new(0.0, 0.9)).collect();
        write_f32_wav(&wav, &samples, 48_000);
        std::fs::write(
            dir.path().join("baseband.json"),
            r#"{"center_freq_hz": 0.0}"#,
        )
        .unwrap();

        let mut src = open_replay_wav(&wav).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
        let all = read_all(&mut *src).unwrap();
        assert_eq!(
            all, samples,
            "a zero-frequency sidecar still declares IQ -- Q must not be \
             discarded and re-synthesized by the AudioIqSource path"
        );
    }

    // The other half of the same split: declaring `0.0` marks the file as
    // IQ but does NOT report a real RF frequency, so the CLI's
    // --dial-freq-hz gate must still fire for it.
    #[test]
    fn a_zero_freq_sidecar_declares_iq_format_but_no_rf_frequency() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("baseband.wav");
        write_f32_wav(&wav, &samples(), 48_000);
        std::fs::write(
            dir.path().join("baseband.json"),
            r#"{"center_freq_hz": 0.0}"#,
        )
        .unwrap();

        assert!(replay_wav_has_iq_sidecar(&wav));
        assert_eq!(replay_wav_center_freq_hz(&wav), None);
    }

    #[test]
    fn replay_wav_has_iq_sidecar_is_false_without_a_parseable_sidecar() {
        let dir = tempfile::tempdir().unwrap();

        let no_sidecar = dir.path().join("nosidecar.wav");
        write_f32_wav(&no_sidecar, &samples(), 48_000);
        assert!(!replay_wav_has_iq_sidecar(&no_sidecar));

        let bad_sidecar = dir.path().join("badsidecar.wav");
        write_f32_wav(&bad_sidecar, &samples(), 48_000);
        std::fs::write(dir.path().join("badsidecar.json"), "not json").unwrap();
        assert!(!replay_wav_has_iq_sidecar(&bad_sidecar));

        let mono = dir.path().join("rig.wav");
        write_mono_f32_wav(&mono, &vec![0.0f32; 480], 48_000);
        std::fs::write(
            dir.path().join("rig.json"),
            r#"{"center_freq_hz": 7030000.0}"#,
        )
        .unwrap();
        assert!(!replay_wav_has_iq_sidecar(&mono));

        // Never fails, for the same reason replay_wav_center_freq_hz never
        // does: the CLI gate probes before any file I/O is meant to fail.
        assert!(!replay_wav_has_iq_sidecar(std::path::Path::new(
            "/nonexistent.wav"
        )));
    }

    #[test]
    fn open_replay_wav_still_rejects_a_mono_file_at_the_wrong_rate() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("rig.wav");
        write_mono_f32_wav(&wav, &vec![0.0f32; 441], 44_100);

        match open_replay_wav(&wav) {
            Ok(_) => panic!("expected an error for a 44100 Hz mono file"),
            Err(err) => assert!(
                format!("{err}").contains("48000"),
                "expected the AudioIqSource rate error, got: {err}"
            ),
        }
    }

    #[test]
    fn replay_wav_center_freq_hz_reports_a_sidecar_backed_iq_file() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("v1.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        std::fs::write(
            dir.path().join("v1.json"),
            r#"{"center_freq_hz": 14000000.0}"#,
        )
        .unwrap();

        assert_eq!(replay_wav_center_freq_hz(&wav), Some(14_000_000.0));
    }

    #[test]
    fn replay_wav_center_freq_hz_is_none_for_mono_missing_and_malformed_inputs() {
        let dir = tempfile::tempdir().unwrap();

        // Mono WAV, no sidecar possible (a real audio source, not IQ).
        let mono = dir.path().join("rig.wav");
        write_mono_f32_wav(&mono, &vec![0.0f32; 480], 48_000);
        assert_eq!(replay_wav_center_freq_hz(&mono), None);

        // 2ch WAV with no sidecar at all.
        let no_sidecar = dir.path().join("nosidecar.wav");
        write_f32_wav(&no_sidecar, &samples(), 96_000);
        assert_eq!(replay_wav_center_freq_hz(&no_sidecar), None);

        // 2ch WAV with an unparseable sidecar.
        let bad_sidecar = dir.path().join("badsidecar.wav");
        write_f32_wav(&bad_sidecar, &samples(), 96_000);
        std::fs::write(dir.path().join("badsidecar.json"), "not json").unwrap();
        assert_eq!(replay_wav_center_freq_hz(&bad_sidecar), None);

        // Nonexistent path -- MUST NOT panic or error, only return None,
        // since a CLI flag-validation gate calls this before any file I/O
        // is meant to fail (crates/manta-cli/tests/cli.rs's
        // server_config_without_dial_freq_for_audio_source_is_a_clean_error
        // depends on this exact ordering).
        assert_eq!(
            replay_wav_center_freq_hz(std::path::Path::new("/nonexistent.wav")),
            None
        );
    }
}
