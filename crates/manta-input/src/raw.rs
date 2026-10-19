//! Headerless interleaved complex IQ files (MAN-309): little-endian I then
//! Q, as complex-float32 (`cf32`) or complex-int16 (`ci16`). Nothing in the
//! file says its rate or centre, so the caller supplies both. A manta IQ
//! recording is a `cf32` payload plus metadata; see `recording_format`.

use crate::{IqSource, MAX_IQ_WAV_RATE_HZ};
use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// The sample layout of a headerless IQ file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIqFormat {
    /// Little-endian IEEE f32 I, then f32 Q: 8 bytes per frame.
    Cf32,
    /// Little-endian signed i16 I, then i16 Q, each divided by 32768.0:
    /// 4 bytes per frame.
    Ci16,
}

impl RawIqFormat {
    /// Bytes per complex frame.
    pub fn frame_bytes(self) -> usize {
        match self {
            RawIqFormat::Cf32 => 8,
            RawIqFormat::Ci16 => 4,
        }
    }

    /// The command-line and config spelling: `cf32` or `ci16`.
    pub fn as_str(self) -> &'static str {
        match self {
            RawIqFormat::Cf32 => "cf32",
            RawIqFormat::Ci16 => "ci16",
        }
    }

    fn decode(self, frame: &[u8]) -> Complex32 {
        match self {
            RawIqFormat::Cf32 => Complex32::new(
                f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]),
                f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
            ),
            RawIqFormat::Ci16 => Complex32::new(
                f32::from(i16::from_le_bytes([frame[0], frame[1]])) / 32768.0,
                f32::from(i16::from_le_bytes([frame[2], frame[3]])) / 32768.0,
            ),
        }
    }
}

impl std::str::FromStr for RawIqFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "cf32" => Ok(RawIqFormat::Cf32),
            "ci16" => Ok(RawIqFormat::Ci16),
            other => Err(format!(
                "unknown IQ format `{other}`; expected cf32 or ci16"
            )),
        }
    }
}

impl std::fmt::Display for RawIqFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The rate check every IQ file reader applies before any rate-sized
/// allocation downstream (MAN-269's `MAX_IQ_WAV_RATE_HZ` bound).
pub fn check_iq_file_rate(path: &Path, sample_rate_hz: f64) -> Result<()> {
    if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 || sample_rate_hz > MAX_IQ_WAV_RATE_HZ {
        bail!(
            "IQ file {} has a sample rate of {sample_rate_hz} Hz; IQ replay needs a rate above 0 \
             and at most {} Hz",
            path.display(),
            MAX_IQ_WAV_RATE_HZ as u64
        );
    }
    Ok(())
}

/// The centre-frequency check for IQ file metadata: finite and not negative.
pub fn check_iq_file_center(path: &Path, center_freq_hz: f64) -> Result<()> {
    if !center_freq_hz.is_finite() || center_freq_hz < 0.0 {
        bail!(
            "IQ file {} has a centre frequency of {center_freq_hz} Hz; it must be finite and \
             not negative",
            path.display()
        );
    }
    Ok(())
}

/// A headerless cf32/ci16 file as an `IqSource`. Streams through a buffered
/// reader, so memory does not grow with the file.
pub struct RawIqSource {
    reader: BufReader<std::fs::File>,
    path: PathBuf,
    format: RawIqFormat,
    fs: f64,
    center_freq_hz: f64,
    bytes: Vec<u8>,
}

impl RawIqSource {
    /// Opens `path` as headerless `format` IQ at `sample_rate_hz`, centred on
    /// `center_freq_hz`. Fails before reading a sample when the rate or
    /// centre is invalid or the file holds a partial complex frame.
    pub fn open(
        path: &Path,
        format: RawIqFormat,
        sample_rate_hz: f64,
        center_freq_hz: f64,
    ) -> Result<Self> {
        check_iq_file_rate(path, sample_rate_hz)?;
        check_iq_file_center(path, center_freq_hz)?;
        let file = std::fs::File::open(path)
            .with_context(|| format!("open IQ file {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("read IQ file {}", path.display()))?
            .len();
        let frame = format.frame_bytes() as u64;
        if len % frame != 0 {
            bail!(
                "IQ file {} is {len} bytes, not a whole number of {}-byte {} frames",
                path.display(),
                frame,
                format
            );
        }
        Ok(RawIqSource {
            reader: BufReader::with_capacity(1 << 16, file),
            path: path.to_path_buf(),
            format,
            fs: sample_rate_hz,
            center_freq_hz,
            bytes: Vec::new(),
        })
    }
}

impl IqSource for RawIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let frame = self.format.frame_bytes();
        // At most 64 Ki frames per underlying fill, so a huge caller buffer
        // never sizes a matching byte buffer in one go.
        let mut n = 0;
        while n < buf.len() {
            let want = (buf.len() - n).min(1 << 16);
            self.bytes.resize(want * frame, 0);
            let mut got = 0;
            while got < self.bytes.len() {
                let r = self
                    .reader
                    .read(&mut self.bytes[got..])
                    .with_context(|| format!("read IQ file {}", self.path.display()))?;
                if r == 0 {
                    break;
                }
                got += r;
            }
            if got % frame != 0 {
                bail!(
                    "IQ file {} ends inside a {}-byte {} frame",
                    self.path.display(),
                    frame,
                    self.format
                );
            }
            for (dst, src) in buf[n..]
                .iter_mut()
                .zip(self.bytes[..got].chunks_exact(frame))
            {
                *dst = self.format.decode(src);
            }
            n += got / frame;
            if got < self.bytes.len() {
                break;
            }
        }
        Ok(n)
    }
}
