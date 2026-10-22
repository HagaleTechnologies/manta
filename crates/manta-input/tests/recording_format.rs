//! MAN-309: the manta IQ recording format and its streaming reader.

use manta_input::recording_format::{MetadataRecord, MAX_METADATA_LINE_BYTES};
use manta_input::{
    metadata_path, open_iq_file, partial_metadata_path, read_recording_summary, EndReason,
    IqFileOptions, IqSource, RecordedIqSource, RecordingHeader,
};
use num_complex::Complex32;
use std::path::{Path, PathBuf};

fn header(fs: f64) -> RecordingHeader {
    RecordingHeader {
        schema: "manta-iq.v1".into(),
        sample_format: "cf32_le".into(),
        sample_rate_hz: fs,
        center_freq_hz: 14_000_000.0,
        start_unix_seconds: 1_792_407_600,
        start_nanoseconds: 250,
        rf_passband_hz: [-fs / 2.0, fs / 2.0],
        trusted_band_hz: None,
        analytic_guard_hz: 0.0,
    }
}

fn samples(n: usize) -> Vec<Complex32> {
    (0..n)
        .map(|i| Complex32::new(i as f32, -(i as f32)))
        .collect()
}

/// Writes a recording pair: `gaps` are `(before_sample, missing)`.
fn write_pair(dir: &Path, h: &RecordingHeader, data: &[Complex32], gaps: &[(u64, u64)]) -> PathBuf {
    let path = dir.join("rec.cf32");
    let mut bytes = Vec::new();
    for s in data {
        bytes.extend_from_slice(&s.re.to_le_bytes());
        bytes.extend_from_slice(&s.im.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();
    let mut meta = MetadataRecord::Header(h.clone()).to_line();
    for &(before_sample, missing_samples) in gaps {
        meta += &MetadataRecord::Gap {
            before_sample,
            missing_samples,
        }
        .to_line();
    }
    meta += &MetadataRecord::End {
        sample_count: data.len() as u64,
        reason: EndReason::Stopped,
    }
    .to_line();
    std::fs::write(metadata_path(&path), meta).unwrap();
    path
}

/// Every read as `(gap taken after it, samples)`.
fn trace(src: &mut dyn IqSource, size: usize) -> Vec<(Option<u64>, Vec<Complex32>)> {
    let mut out = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); size];
    loop {
        let n = src.read(&mut buf).unwrap();
        out.push((src.take_discontinuity(), buf[..n].to_vec()));
        if n == 0 {
            return out;
        }
    }
}

/// `(stored position, missing)` for every reported gap, plus all samples.
fn gaps_and_samples(t: &[(Option<u64>, Vec<Complex32>)]) -> (Vec<(u64, u64)>, Vec<Complex32>) {
    let mut gaps = Vec::new();
    let mut all = Vec::new();
    for (gap, s) in t {
        if let Some(g) = gap {
            gaps.push((all.len() as u64, *g));
        }
        all.extend_from_slice(s);
    }
    (gaps, all)
}

#[test]
fn metadata_path_appends_to_the_whole_path() {
    assert_eq!(
        metadata_path(Path::new("a/session.cf32")),
        PathBuf::from("a/session.cf32.meta.jsonl")
    );
    assert_eq!(
        partial_metadata_path(Path::new("a/session.cf32")),
        PathBuf::from("a/session.cf32.meta.jsonl.partial")
    );
}

#[test]
fn header_fields_round_trip_and_restore_source_properties() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = header(48_000.0);
    h.rf_passband_hz = [0.0, 3_000.0];
    h.trusted_band_hz = Some([-2_600.0, 2_600.0]);
    h.analytic_guard_hz = 600.0;
    let path = write_pair(dir.path(), &h, &samples(10), &[]);
    let src = RecordedIqSource::open(&path).unwrap();
    assert_eq!(src.summary().header, h);
    assert_eq!(src.sample_rate(), 48_000.0);
    assert_eq!(src.center_freq_hz(), 14_000_000.0);
    assert_eq!(src.rf_passband_hz(), (0.0, 3_000.0));
    assert_eq!(src.trusted_band_hz(), Some((-2_600.0, 2_600.0)));
    assert_eq!(src.analytic_guard_hz(), 600.0);
    assert_eq!(
        h.start_time().unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::new(1_792_407_600, 250)
    );
}

#[test]
fn gaps_are_delivered_once_at_their_positions_under_any_read_size() {
    let dir = tempfile::tempdir().unwrap();
    let data = samples(10_000);
    // At zero, inside reads, two close together, zero-length and terminal.
    let gaps = [(0, 5), (4097, 96_000), (4099, 0), (7000, 12), (10_000, 480)];
    let path = write_pair(dir.path(), &header(96_000.0), &data, &gaps);
    for size in [1, 7, 4095, 4096, 4097, 20_000] {
        let mut src = RecordedIqSource::open(&path).unwrap();
        let t = trace(&mut src, size);
        let (got_gaps, got) = gaps_and_samples(&t);
        assert_eq!(got, data, "size {size}");
        assert_eq!(got_gaps, gaps.to_vec(), "size {size}");
        // The terminal gap rides on the EOF read.
        assert_eq!(t.last().unwrap(), &(Some(480), Vec::new()));
    }
}

#[test]
fn a_centre_override_keeps_metadata_and_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pair(dir.path(), &header(48_000.0), &samples(100), &[(50, 7)]);
    let mut src = open_iq_file(
        &path,
        IqFileOptions {
            center_freq_hz: Some(7_000_000.0),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(src.center_freq_hz(), 7_000_000.0);
    let (gaps, _) = gaps_and_samples(&trace(src.as_mut(), 64));
    assert_eq!(gaps, vec![(50, 7)]);
}

#[test]
fn explicit_format_or_rate_must_match_the_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pair(dir.path(), &header(48_000.0), &samples(10), &[]);
    let wrong_rate = IqFileOptions {
        raw_format: Some(manta_input::RawIqFormat::Cf32),
        sample_rate_hz: Some(96_000.0),
        center_freq_hz: Some(1.0),
    };
    assert!(open_iq_file(&path, wrong_rate).is_err());
    let wrong_format = IqFileOptions {
        raw_format: Some(manta_input::RawIqFormat::Ci16),
        sample_rate_hz: Some(48_000.0),
        center_freq_hz: Some(1.0),
    };
    assert!(open_iq_file(&path, wrong_format).is_err());
}

fn expect_err(path: &Path, needle: &str) {
    let err = format!(
        "{:#}",
        RecordedIqSource::open(path).err().expect("must fail")
    );
    assert!(err.contains(needle), "{err}");
    // A centre override cannot bypass the validation.
    assert!(open_iq_file(
        path,
        IqFileOptions {
            center_freq_hz: Some(1.0),
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn malformed_metadata_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pair(dir.path(), &header(48_000.0), &samples(10), &[(3, 1)]);
    let meta = metadata_path(&path);
    let good = std::fs::read_to_string(&meta).unwrap();
    let lines: Vec<&str> = good.lines().collect();
    let cases: Vec<(String, &str)> = vec![
        (
            good.replace("manta-iq.v1", "manta-iq.v2"),
            "unsupported recording schema",
        ),
        (
            good.replace("cf32_le", "ci16_le"),
            "unsupported sample_format",
        ),
        (
            good.replace(
                "\"start_nanoseconds\":250",
                "\"start_nanoseconds\":1000000000",
            ),
            "start_nanoseconds",
        ),
        (format!("{}\n{}\n", lines[0], lines[1]), "no end record"),
        (format!("{}\n{}\n", lines[1], lines[2]), "not a header"),
        (
            format!("{}\n{}\n{}\n", lines[0], lines[0], lines[2]),
            "second header",
        ),
        (format!("{good}{}\n", lines[1]), "after the end record"),
        (
            good.replace("\"type\":\"gap\"", "\"type\":\"note\""),
            "line 2",
        ),
        (good.replace("{\"type\":\"end\"", "{oops"), "line 3"),
        (
            good.replace("\"sample_count\":10", "\"sample_count\":11"),
            "bytes",
        ),
        (
            good.replace("\"before_sample\":3", "\"before_sample\":11"),
            "beyond sample_count",
        ),
        (
            good.replace(
                "\"sample_count\":10",
                "\"sample_count\":18446744073709551615",
            ),
            "overflows",
        ),
        (
            good.replace("\"sample_rate_hz\":48000.0", "\"sample_rate_hz\":0.0"),
            "sample rate",
        ),
        (
            good.replace("\"analytic_guard_hz\":0.0", "\"analytic_guard_hz\":-1.0"),
            "analytic_guard_hz",
        ),
        (
            good.replace("[-24000.0,24000.0]", "[1000.0,-1000.0]"),
            "rf_passband_hz",
        ),
        // Bands must lie inside the stored spectrum.
        (
            good.replace("[-24000.0,24000.0]", "[-30000.0,24000.0]"),
            "rf_passband_hz",
        ),
        (
            good.replace(
                "\"trusted_band_hz\":null",
                "\"trusted_band_hz\":[-1000.0,25000.0]",
            ),
            "trusted_band_hz",
        ),
        (
            good.replace(
                "\"trusted_band_hz\":null",
                "\"trusted_band_hz\":[500.0,-500.0]",
            ),
            "trusted_band_hz",
        ),
        (
            format!("{}\n", "x".repeat(MAX_METADATA_LINE_BYTES + 10)),
            "longer than",
        ),
        // A gap too long for any sample clock (MAN-309 review).
        (
            good.replace(
                "\"missing_samples\":1",
                "\"missing_samples\":18446744073709551615",
            ),
            "gaps exceed",
        ),
        (
            good.replace(
                "\"missing_samples\":1",
                "\"missing_samples\":9007199254740990",
            ),
            "spans more than",
        ),
    ];
    for (text, needle) in cases {
        std::fs::write(&meta, &text).unwrap();
        expect_err(&path, needle);
    }
    // Gaps must strictly increase.
    std::fs::write(
        &meta,
        format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[1], lines[2]),
    )
    .unwrap();
    expect_err(&path, "strictly increase");
}

#[test]
fn a_decimated_recording_keeps_a_guard_wider_than_its_nyquist() {
    // MAN-273: rig audio decimated to 6 kS/s keeps the 48 kS/s analytic
    // guard; it vetoes nothing new there, and the recording keeps it.
    let dir = tempfile::tempdir().unwrap();
    let mut h = header(6_000.0);
    h.analytic_guard_hz = 4_000.0;
    h.trusted_band_hz = Some([-2_500.0, 2_500.0]);
    let path = write_pair(dir.path(), &h, &samples(10), &[]);
    let src = RecordedIqSource::open(&path).unwrap();
    assert_eq!(src.analytic_guard_hz(), 4_000.0);
    assert_eq!(src.trusted_band_hz(), Some((-2_500.0, 2_500.0)));
}

#[test]
fn a_partial_marker_is_an_incomplete_recording() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_pair(dir.path(), &header(48_000.0), &samples(10), &[]);
    std::fs::rename(metadata_path(&path), partial_metadata_path(&path)).unwrap();
    expect_err(&path, "incomplete IQ recording");
    assert!(manta_input::is_native_recording(&path));
}

#[test]
fn a_sparse_payload_above_4_gib_opens_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.cf32");
    let count: u64 = (5u64 << 30) / 8;
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(count * 8).unwrap();
    let mut meta = MetadataRecord::Header(header(192_000.0)).to_line();
    // A long metadata stream: read line by line, never collected.
    for i in 0..20_000u64 {
        meta += &MetadataRecord::Gap {
            before_sample: i * 1000,
            missing_samples: 1,
        }
        .to_line();
    }
    meta += &MetadataRecord::End {
        sample_count: count,
        reason: EndReason::Eof,
    }
    .to_line();
    std::fs::write(metadata_path(&path), meta).unwrap();
    let summary = read_recording_summary(&path).unwrap();
    assert_eq!(summary.sample_count, count);
    assert_eq!(summary.gap_count, 20_000);
    assert!(summary.has_gaps());
    let mut src = RecordedIqSource::open(&path).unwrap();
    let mut buf = vec![Complex32::new(1.0, 1.0); 1500];
    assert_eq!(src.read(&mut buf).unwrap(), 1000);
    assert_eq!(src.take_discontinuity(), Some(1));
}
