//! MAN-309: headerless cf32/ci16 IQ files.

use manta_input::{open_iq_file, IqFileOptions, IqSource, RawIqFormat, RawIqSource};
use num_complex::Complex32;
use std::path::Path;

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

fn read_with(src: &mut dyn IqSource, size: usize) -> Vec<Complex32> {
    let mut out = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); size];
    loop {
        let n = src.read(&mut buf).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn cf32_reads_little_endian_i_then_q_bit_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.cf32");
    let samples: Vec<Complex32> = (0..10_000)
        .map(|i| Complex32::new(i as f32 * 0.25, -(i as f32) - 0.5))
        .collect();
    let mut bytes = Vec::new();
    for s in &samples {
        bytes.extend_from_slice(&s.re.to_le_bytes());
        bytes.extend_from_slice(&s.im.to_le_bytes());
    }
    write(&path, &bytes);
    for size in [1, 7, 4095, 4096, 4097] {
        let mut src = RawIqSource::open(&path, RawIqFormat::Cf32, 96_000.0, 14e6).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14e6);
        assert_eq!(src.rf_passband_hz(), (-48_000.0, 48_000.0));
        assert_eq!(src.analytic_guard_hz(), 0.0);
        let got = read_with(&mut src, size);
        assert_eq!(got.len(), samples.len());
        assert!(got
            .iter()
            .zip(&samples)
            .all(|(a, b)| a.re.to_bits() == b.re.to_bits() && a.im.to_bits() == b.im.to_bits()));
        assert_eq!(src.take_discontinuity(), None);
    }
}

#[test]
fn ci16_scales_extrema_by_32768() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.ci16");
    let mut bytes = Vec::new();
    for (i, q) in [(i16::MIN, i16::MAX), (0, -1), (16384, -16384)] {
        bytes.extend_from_slice(&i.to_le_bytes());
        bytes.extend_from_slice(&q.to_le_bytes());
    }
    write(&path, &bytes);
    let mut src = RawIqSource::open(&path, RawIqFormat::Ci16, 48_000.0, 7e6).unwrap();
    let got = read_with(&mut src, 2);
    assert_eq!(
        got,
        vec![
            Complex32::new(-1.0, 32767.0 / 32768.0),
            Complex32::new(0.0, -1.0 / 32768.0),
            Complex32::new(0.5, -0.5),
        ]
    );
}

#[test]
fn empty_file_is_immediate_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.cf32");
    write(&path, &[]);
    let mut src = RawIqSource::open(&path, RawIqFormat::Cf32, 48_000.0, 0.0).unwrap();
    assert!(read_with(&mut src, 4096).is_empty());
}

#[test]
fn partial_frames_are_rejected_at_open_with_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("odd.cf32");
    write(&path, &[0u8; 12]);
    let err = RawIqSource::open(&path, RawIqFormat::Cf32, 48_000.0, 0.0)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("odd.cf32") && err.contains("not a whole number"),
        "{err}"
    );
    let path16 = dir.path().join("odd.ci16");
    write(&path16, &[0u8; 6]);
    assert!(RawIqSource::open(&path16, RawIqFormat::Ci16, 48_000.0, 0.0).is_err());
}

#[test]
fn invalid_rate_and_centre_are_rejected_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.cf32");
    write(&path, &[0u8; 8]);
    for fs in [0.0, -1.0, f64::NAN, f64::INFINITY, 10_000_001.0] {
        assert!(
            RawIqSource::open(&path, RawIqFormat::Cf32, fs, 0.0).is_err(),
            "{fs}"
        );
    }
    assert!(RawIqSource::open(&path, RawIqFormat::Cf32, 10_000_000.0, 0.0).is_ok());
    for c in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(RawIqSource::open(&path, RawIqFormat::Cf32, 48_000.0, c).is_err());
    }
}

#[test]
fn format_names_parse() {
    assert_eq!("cf32".parse::<RawIqFormat>().unwrap(), RawIqFormat::Cf32);
    assert_eq!("ci16".parse::<RawIqFormat>().unwrap(), RawIqFormat::Ci16);
    assert!("cs8".parse::<RawIqFormat>().is_err());
}

#[test]
fn open_iq_file_requires_rate_and_centre_for_headerless_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.cf32");
    write(&path, &[0u8; 8]);
    let only_format = IqFileOptions {
        raw_format: Some(RawIqFormat::Cf32),
        ..Default::default()
    };
    assert!(open_iq_file(&path, only_format).is_err());
    let full = IqFileOptions {
        raw_format: Some(RawIqFormat::Cf32),
        sample_rate_hz: Some(96_000.0),
        center_freq_hz: Some(14e6),
    };
    let src = open_iq_file(&path, full).unwrap();
    assert_eq!(src.sample_rate(), 96_000.0);
    // Without --iq-format the file is a WAV, and this one is not.
    assert!(open_iq_file(&path, IqFileOptions::default()).is_err());
}
