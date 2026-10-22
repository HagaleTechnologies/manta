//! The manta IQ recording format (MAN-309): a headerless little-endian cf32
//! payload at `PATH`, plus `PATH.meta.jsonl`, a JSON Lines document holding
//! one `header` record, zero or more `gap` records and one `end` record.
//! While a recording is being written its metadata lives at
//! `PATH.meta.jsonl.partial`; the writer renames it only after the end
//! record is flushed. See docs/GUIDES/iq-recording.md.

use crate::raw::{check_iq_file_center, check_iq_file_rate, RawIqFormat, RawIqSource};
use crate::IqSource;
use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// The `schema` value of a v1 header.
pub const RECORDING_SCHEMA: &str = "manta-iq.v1";
/// The `sample_format` value of a v1 header.
pub const RECORDING_SAMPLE_FORMAT: &str = "cf32_le";
/// The longest metadata line a reader accepts, in bytes.
pub const MAX_METADATA_LINE_BYTES: usize = 64 * 1024;
/// The most samples a recording may span, stored plus missing: 2^53, about
/// 28 years at 10 MS/s. Keeps every sample clock built from it (the
/// engine's included) far from overflow.
pub const MAX_RECORDING_SPAN_SAMPLES: u64 = 1 << 53;

/// `PATH.meta.jsonl`: the suffix is appended to the whole path.
pub fn metadata_path(payload: &Path) -> PathBuf {
    let mut s = payload.as_os_str().to_os_string();
    s.push(".meta.jsonl");
    PathBuf::from(s)
}

/// `PATH.meta.jsonl.partial`: the metadata of a recording still being
/// written, or one whose writer never finalized it.
pub fn partial_metadata_path(payload: &Path) -> PathBuf {
    let mut s = metadata_path(payload).into_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

/// Whether `payload` has manta recording metadata next to it, complete or
/// partial. Opening one with only the partial marker fails.
pub fn is_native_recording(payload: &Path) -> bool {
    metadata_path(payload).exists() || partial_metadata_path(payload).exists()
}

/// Why a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    /// The recorded source reached its end.
    Eof,
    /// The session stopped (signal, decode error or shutdown).
    Stopped,
    /// The writer fell behind; the file holds a contiguous prefix only.
    Backpressure,
}

/// The first metadata record.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecordingHeader {
    pub schema: String,
    pub sample_format: String,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    /// Wall-clock time of stored sample zero: whole seconds since the Unix
    /// epoch, then nanoseconds in `[0, 1e9)`.
    pub start_unix_seconds: i64,
    pub start_nanoseconds: u32,
    /// `IqSource::rf_passband_hz` of the recorded source.
    pub rf_passband_hz: [f64; 2],
    /// `IqSource::trusted_band_hz` of the recorded source.
    pub trusted_band_hz: Option<[f64; 2]>,
    /// `IqSource::analytic_guard_hz` of the recorded source.
    pub analytic_guard_hz: f64,
}

impl RecordingHeader {
    /// The header describing `src`, with sample zero at `start`.
    pub fn for_source<S: IqSource + ?Sized>(src: &S, start: std::time::SystemTime) -> Self {
        let (secs, nanos) = match start.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                // Before 1970: floor to whole seconds, keep nanoseconds positive.
                let d = e.duration();
                let mut secs = -(d.as_secs() as i64);
                let mut nanos = d.subsec_nanos();
                if nanos > 0 {
                    secs -= 1;
                    nanos = 1_000_000_000 - nanos;
                }
                (secs, nanos)
            }
        };
        let (lo, hi) = src.rf_passband_hz();
        RecordingHeader {
            schema: RECORDING_SCHEMA.to_string(),
            sample_format: RECORDING_SAMPLE_FORMAT.to_string(),
            sample_rate_hz: src.sample_rate(),
            center_freq_hz: src.center_freq_hz(),
            start_unix_seconds: secs,
            start_nanoseconds: nanos,
            rf_passband_hz: [lo, hi],
            trusted_band_hz: src.trusted_band_hz().map(|(lo, hi)| [lo, hi]),
            analytic_guard_hz: src.analytic_guard_hz(),
        }
    }

    /// Wall-clock time of stored sample zero.
    pub fn start_time(&self) -> Option<std::time::SystemTime> {
        let nanos = std::time::Duration::from_nanos(u64::from(self.start_nanoseconds));
        if self.start_unix_seconds >= 0 {
            std::time::UNIX_EPOCH
                .checked_add(std::time::Duration::from_secs(
                    self.start_unix_seconds as u64,
                ))?
                .checked_add(nanos)
        } else {
            std::time::UNIX_EPOCH
                .checked_sub(std::time::Duration::from_secs(
                    self.start_unix_seconds.unsigned_abs(),
                ))?
                .checked_add(nanos)
        }
    }

    fn validate(&self, meta: &Path) -> Result<()> {
        if self.schema != RECORDING_SCHEMA {
            bail!(
                "{}: unsupported recording schema `{}` (this manta reads {RECORDING_SCHEMA})",
                meta.display(),
                self.schema
            );
        }
        if self.sample_format != RECORDING_SAMPLE_FORMAT {
            bail!(
                "{}: unsupported sample_format `{}` (expected {RECORDING_SAMPLE_FORMAT})",
                meta.display(),
                self.sample_format
            );
        }
        check_iq_file_rate(meta, self.sample_rate_hz)?;
        check_iq_file_center(meta, self.center_freq_hz)?;
        if self.start_nanoseconds >= 1_000_000_000 {
            bail!(
                "{}: start_nanoseconds {} is not below 1000000000",
                meta.display(),
                self.start_nanoseconds
            );
        }
        if self.start_time().is_none() {
            bail!("{}: start time is not representable", meta.display());
        }
        let half = self.sample_rate_hz / 2.0;
        let band_ok = |[lo, hi]: [f64; 2]| {
            lo.is_finite() && hi.is_finite() && lo < hi && lo >= -half - 1e-6 && hi <= half + 1e-6
        };
        if !band_ok(self.rf_passband_hz) {
            let [lo, hi] = self.rf_passband_hz;
            bail!(
                "{}: rf_passband_hz [{lo}, {hi}] is not an ordered band inside +/-{half} Hz",
                meta.display()
            );
        }
        if let Some(band @ [lo, hi]) = self.trusted_band_hz {
            if !band_ok(band) {
                bail!(
                    "{}: trusted_band_hz [{lo}, {hi}] is not an ordered band inside +/-{half} Hz",
                    meta.display()
                );
            }
        }
        if !self.analytic_guard_hz.is_finite() || self.analytic_guard_hz < 0.0 {
            bail!(
                "{}: analytic_guard_hz {} must be finite and not negative",
                meta.display(),
                self.analytic_guard_hz
            );
        }
        Ok(())
    }
}

/// One line of the metadata document.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MetadataRecord {
    Header(RecordingHeader),
    /// `missing_samples` were lost immediately before stored frame
    /// `before_sample`; `before_sample == sample_count` is a terminal gap.
    Gap {
        before_sample: u64,
        missing_samples: u64,
    },
    End {
        sample_count: u64,
        reason: EndReason,
    },
}

impl MetadataRecord {
    /// One JSON Lines line, newline included.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("metadata record serializes");
        line.push('\n');
        line
    }
}

/// What the validation pass learned about a recording.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingSummary {
    pub header: RecordingHeader,
    pub sample_count: u64,
    pub reason: EndReason,
    pub gap_count: u64,
}

impl RecordingSummary {
    /// Whether the recording holds any discontinuity, terminal or zero-length
    /// ones included.
    pub fn has_gaps(&self) -> bool {
        self.gap_count > 0
    }
}

/// Reads one `\n`-terminated line of at most `MAX_METADATA_LINE_BYTES`.
fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, meta: &Path) -> Result<bool> {
    buf.clear();
    let n = reader
        .by_ref()
        .take(MAX_METADATA_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buf)
        .with_context(|| format!("read {}", meta.display()))?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if n > MAX_METADATA_LINE_BYTES {
        bail!(
            "{}: a line is longer than {MAX_METADATA_LINE_BYTES} bytes",
            meta.display()
        );
    }
    if buf.len() > MAX_METADATA_LINE_BYTES {
        bail!(
            "{}: a line is longer than {MAX_METADATA_LINE_BYTES} bytes",
            meta.display()
        );
    }
    Ok(true)
}

fn parse_record(buf: &[u8], meta: &Path, line_no: u64) -> Result<MetadataRecord> {
    serde_json::from_slice(buf).with_context(|| format!("{} line {line_no}", meta.display()))
}

/// Validates `payload`'s metadata in one bounded-memory pass and checks the
/// payload length against it. Fails when the metadata is missing, only
/// partial, or malformed.
pub fn read_recording_summary(payload: &Path) -> Result<RecordingSummary> {
    let meta = metadata_path(payload);
    if !meta.exists() {
        let partial = partial_metadata_path(payload);
        if partial.exists() {
            bail!("incomplete IQ recording: {}", partial.display());
        }
        bail!(
            "IQ recording {} has no metadata file {}",
            payload.display(),
            meta.display()
        );
    }
    let file = std::fs::File::open(&meta).with_context(|| format!("open {}", meta.display()))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut line_no = 0u64;
    let mut header: Option<RecordingHeader> = None;
    let mut end: Option<(u64, EndReason)> = None;
    let mut gap_count = 0u64;
    let mut last_gap: Option<u64> = None;
    let mut missing_total = 0u64;
    while read_line(&mut reader, &mut buf, &meta)? {
        line_no += 1;
        let record = parse_record(&buf, &meta, line_no)?;
        if end.is_some() {
            bail!(
                "{} line {line_no}: record after the end record",
                meta.display()
            );
        }
        match record {
            MetadataRecord::Header(h) => {
                if header.is_some() {
                    bail!("{} line {line_no}: a second header", meta.display());
                }
                h.validate(&meta)?;
                header = Some(h);
            }
            _ if header.is_none() => {
                bail!(
                    "{} line {line_no}: the first record is not a header",
                    meta.display()
                );
            }
            MetadataRecord::Gap {
                before_sample,
                missing_samples,
            } => {
                missing_total = missing_total
                    .checked_add(missing_samples)
                    .filter(|t| *t <= MAX_RECORDING_SPAN_SAMPLES)
                    .with_context(|| {
                        format!(
                            "{} line {line_no}: the gaps exceed {MAX_RECORDING_SPAN_SAMPLES} \
                             missing samples",
                            meta.display()
                        )
                    })?;
                if last_gap.is_some_and(|last| before_sample <= last) {
                    bail!(
                        "{} line {line_no}: gap positions must strictly increase",
                        meta.display()
                    );
                }
                last_gap = Some(before_sample);
                gap_count += 1;
            }
            MetadataRecord::End {
                sample_count,
                reason,
            } => end = Some((sample_count, reason)),
        }
    }
    let Some(header) = header else {
        bail!("{}: no header record", meta.display());
    };
    let Some((sample_count, reason)) = end else {
        bail!("{}: no end record", meta.display());
    };
    if last_gap.is_some_and(|last| last > sample_count) {
        bail!(
            "{}: a gap lies beyond sample_count {sample_count}",
            meta.display()
        );
    }
    let want = sample_count
        .checked_mul(RawIqFormat::Cf32.frame_bytes() as u64)
        .with_context(|| format!("{}: sample_count overflows", meta.display()))?;
    if sample_count
        .checked_add(missing_total)
        .is_none_or(|span| span > MAX_RECORDING_SPAN_SAMPLES)
    {
        bail!(
            "{}: the recording spans more than {MAX_RECORDING_SPAN_SAMPLES} samples",
            meta.display()
        );
    }
    let len = std::fs::metadata(payload)
        .with_context(|| format!("open IQ recording {}", payload.display()))?
        .len();
    if len != want {
        bail!(
            "IQ recording {} is {len} bytes, but {} says {sample_count} samples ({want} bytes)",
            payload.display(),
            meta.display()
        );
    }
    Ok(RecordingSummary {
        header,
        sample_count,
        reason,
        gap_count,
    })
}

/// A finished manta IQ recording as an `IqSource`: the payload's samples,
/// the recorded source's passband and guards, and each recorded gap once
/// through `take_discontinuity`. A read never spans a gap.
pub struct RecordedIqSource {
    raw: RawIqSource,
    summary: RecordingSummary,
    center_freq_hz: f64,
    meta_path: PathBuf,
    meta: BufReader<std::fs::File>,
    line: Vec<u8>,
    line_no: u64,
    next_gap: Option<(u64, u64)>,
    pos: u64,
    pending: Option<u64>,
}

impl RecordedIqSource {
    /// Validates and opens the recording at `payload`.
    pub fn open(payload: &Path) -> Result<Self> {
        let summary = read_recording_summary(payload)?;
        let meta_path = metadata_path(payload);
        let file = std::fs::File::open(&meta_path)
            .with_context(|| format!("open {}", meta_path.display()))?;
        let raw = RawIqSource::open(
            payload,
            RawIqFormat::Cf32,
            summary.header.sample_rate_hz,
            summary.header.center_freq_hz,
        )?;
        let mut src = RecordedIqSource {
            raw,
            center_freq_hz: summary.header.center_freq_hz,
            summary,
            meta_path,
            meta: BufReader::new(file),
            line: Vec::new(),
            line_no: 0,
            next_gap: None,
            pos: 0,
            pending: None,
        };
        src.advance_gap()?;
        Ok(src)
    }

    /// Replaces the recorded centre frequency (the RF origin only).
    pub fn with_center_freq_hz(mut self, center_freq_hz: f64) -> Self {
        self.center_freq_hz = center_freq_hz;
        self
    }

    /// The validated metadata summary.
    pub fn summary(&self) -> &RecordingSummary {
        &self.summary
    }

    /// Moves `next_gap` to the next gap record, `None` after the last.
    fn advance_gap(&mut self) -> Result<()> {
        self.next_gap = None;
        while read_line(&mut self.meta, &mut self.line, &self.meta_path)? {
            self.line_no += 1;
            if let MetadataRecord::Gap {
                before_sample,
                missing_samples,
            } = parse_record(&self.line, &self.meta_path, self.line_no)?
            {
                self.next_gap = Some((before_sample, missing_samples));
                return Ok(());
            }
        }
        Ok(())
    }
}

impl IqSource for RecordedIqSource {
    fn sample_rate(&self) -> f64 {
        self.summary.header.sample_rate_hz
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        let [lo, hi] = self.summary.header.rf_passband_hz;
        (lo, hi)
    }

    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.summary.header.trusted_band_hz.map(|[lo, hi]| (lo, hi))
    }

    fn analytic_guard_hz(&self) -> f64 {
        self.summary.header.analytic_guard_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if let Some((at, missing)) = self.next_gap {
            if at == self.pos {
                self.pending = Some(
                    self.pending
                        .unwrap_or(0)
                        .checked_add(missing)
                        .context("recorded gap length overflows")?,
                );
                self.advance_gap()?;
            }
        }
        let limit = match self.next_gap {
            Some((at, _)) => buf
                .len()
                .min(usize::try_from(at - self.pos).unwrap_or(usize::MAX)),
            None => buf.len(),
        };
        let n = self.raw.read(&mut buf[..limit])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
}
