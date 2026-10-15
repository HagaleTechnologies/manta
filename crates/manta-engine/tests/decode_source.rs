//! MAN-119: `decode_source` streams a source through the same pipeline as
//! `decode_samples`, with the same 4096-sample chunks of the zero-padded
//! stream whatever sizes the source's reads return.

use anyhow::{bail, Result};
use manta_engine::{decode_samples, decode_source, PipelineConfig};
use manta_input::IqSource;
use num_complex::Complex32;

/// A source that returns at most `step` samples per read, and fails with
/// "stepped source failed" once `fail_after` samples have been read.
struct Stepped<'a> {
    iq: &'a [Complex32],
    fs: f64,
    center: f64,
    step: usize,
    fail_after: Option<usize>,
    read: usize,
}

impl<'a> Stepped<'a> {
    fn new(iq: &'a [Complex32], fs: f64, center: f64, step: usize) -> Self {
        Stepped {
            iq,
            fs,
            center,
            step,
            fail_after: None,
            read: 0,
        }
    }
}

impl IqSource for Stepped<'_> {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if self.fail_after.is_some_and(|limit| self.read >= limit) {
            bail!("stepped source failed");
        }
        let n = self.step.min(buf.len()).min(self.iq.len() - self.read);
        buf[..n].copy_from_slice(&self.iq[self.read..self.read + n]);
        self.read += n;
        Ok(n)
    }
}

/// The report as text. `serde_json` is not a manta-engine dependency;
/// `Debug` prints every field (the serde-skipped ones too) and each float
/// in its shortest round-trip form, so equal `Debug` text implies equal
/// JSON.
fn report_text(report: &manta_engine::DecodeReport) -> String {
    format!("{report:?}")
}

#[test]
fn decode_source_matches_decode_samples_for_any_read_size() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let cfg = PipelineConfig::default();
    let want =
        report_text(&decode_samples(&r.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap());
    assert!(want.contains("W1AW"), "{want}");
    for step in [1, 7, 4095, 4096, 4097, 65_536] {
        let mut src = Stepped::new(&r.samples, spec.fs, spec.center_freq_hz, step);
        let got = decode_source(&mut src, &cfg).unwrap();
        assert_eq!(report_text(&got), want, "read size {step}");
    }
}

#[test]
fn decode_source_reports_digital_silence() {
    let iq = vec![Complex32::new(0.0, 0.0); 96_000];
    let mut src = Stepped::new(&iq, 96_000.0, 14_000_000.0, 4096);
    let err = format!(
        "{:#}",
        decode_source(&mut src, &PipelineConfig::default()).unwrap_err()
    );
    assert!(err.contains("input is digital silence"), "{err}");
}

#[test]
fn decode_source_reports_an_empty_source_as_digital_silence() {
    let cfg = PipelineConfig::default();
    let want = format!(
        "{:#}",
        decode_samples(&[], 96_000.0, 14_000_000.0, &cfg).unwrap_err()
    );
    assert_eq!(want, "input is digital silence");
    let mut src = Stepped::new(&[], 96_000.0, 14_000_000.0, 4096);
    let got = format!("{:#}", decode_source(&mut src, &cfg).unwrap_err());
    assert_eq!(got, want);
}

#[test]
fn decode_source_propagates_a_read_error() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 2.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let mut src = Stepped::new(&r.samples, spec.fs, spec.center_freq_hz, 4096);
    src.fail_after = Some(50_000);
    let err = format!(
        "{:#}",
        decode_source(&mut src, &PipelineConfig::default()).unwrap_err()
    );
    assert!(err.contains("stepped source failed"), "{err}");
}
