//! MAN-309: recording a source while it is read, and replaying the pair.

use anyhow::Result;
use manta_input::{
    metadata_path, partial_metadata_path, start_recording, EndReason, IqSource, RecordedIqSource,
    RecordingOutcome,
};
use num_complex::Complex32;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

/// One scripted read: `(gap, samples)`, or an error.
type ScriptedRead = std::result::Result<(Option<u64>, Vec<Complex32>), String>;
/// One traced read: `(n or error, gap, sample bits)`.
type TracedRead = (std::result::Result<usize, String>, Option<u64>, Vec<(u32, u32)>);

/// Replays scripted reads.
struct Scripted {
    reads: VecDeque<ScriptedRead>,
    pending: Option<u64>,
    guard: f64,
}

impl Scripted {
    fn new(reads: Vec<(Option<u64>, Vec<Complex32>)>) -> Self {
        Scripted {
            reads: reads.into_iter().map(Ok).collect(),
            pending: None,
            guard: 600.0,
        }
    }
}

impl IqSource for Scripted {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        14_000_000.0
    }
    fn rf_passband_hz(&self) -> (f64, f64) {
        (0.0, 3_000.0)
    }
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        Some((-20_000.0, 20_000.0))
    }
    fn analytic_guard_hz(&self) -> f64 {
        self.guard
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        match self.reads.pop_front() {
            None => Ok(0),
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            Some(Ok((gap, s))) => {
                assert!(s.len() <= buf.len());
                buf[..s.len()].copy_from_slice(&s);
                self.pending = gap;
                Ok(s.len())
            }
        }
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
}

fn block(start: usize, n: usize) -> Vec<Complex32> {
    (start..start + n)
        .map(|i| Complex32::new(i as f32 * 0.5, f32::from_bits(0x7fc0_0000 | i as u32)))
        .collect()
}

fn bits(s: &[Complex32]) -> Vec<(u32, u32)> {
    s.iter().map(|c| (c.re.to_bits(), c.im.to_bits())).collect()
}

/// Every read as `(n or error, gap, sample bits)`.
fn trace(
    src: &mut dyn IqSource,
) -> Vec<TracedRead> {
    let mut out = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); 20_000];
    loop {
        match src.read(&mut buf) {
            Ok(n) => {
                out.push((Ok(n), src.take_discontinuity(), bits(&buf[..n])));
                if n == 0 {
                    return out;
                }
            }
            Err(e) => {
                out.push((Err(e.to_string()), None, Vec::new()));
                return out;
            }
        }
    }
}

fn script() -> Vec<(Option<u64>, Vec<Complex32>)> {
    vec![
        (Some(5), block(0, 10)),
        // A large calibration-sized read spanning several writer blocks.
        (None, block(10, 9000)),
        (Some(96_000), block(9010, 3)),
        (Some(0), block(9013, 4096)),
        // EOF during an outage.
        (Some(480), Vec::new()),
    ]
}

#[test]
fn a_recorded_source_reads_exactly_like_the_bare_one_and_replays_its_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.cf32");
    let bare = trace(&mut Scripted::new(script()));
    let start = SystemTime::UNIX_EPOCH + Duration::new(1_792_407_600, 123);
    let (mut rec, handle) =
        start_recording(Box::new(Scripted::new(script())), &path, start, None).unwrap();
    assert_eq!(rec.sample_rate(), 48_000.0);
    assert_eq!(rec.center_freq_hz(), 14_000_000.0);
    assert_eq!(rec.rf_passband_hz(), (0.0, 3_000.0));
    assert_eq!(rec.trusted_band_hz(), Some((-20_000.0, 20_000.0)));
    assert_eq!(rec.analytic_guard_hz(), 600.0);
    assert_eq!(trace(&mut rec), bare);
    drop(rec);
    let outcome = handle.finish(Duration::from_secs(5));
    assert_eq!(
        outcome,
        RecordingOutcome::Finished {
            path: path.clone(),
            samples: 13_109,
            reason: EndReason::Eof,
        }
    );
    assert!(!partial_metadata_path(&path).exists());
    let mut replay = RecordedIqSource::open(&path).unwrap();
    let h = &replay.summary().header;
    assert_eq!(h.start_unix_seconds, 1_792_407_600);
    assert_eq!(h.start_nanoseconds, 123);
    assert_eq!(h.analytic_guard_hz, 600.0);
    assert_eq!(h.rf_passband_hz, [0.0, 3_000.0]);
    assert_eq!(h.trusted_band_hz, Some([-20_000.0, 20_000.0]));
    assert_eq!(replay.analytic_guard_hz(), 600.0);
    // The replay splits reads at gaps, so compare positions and samples.
    let flatten = |t: Vec<TracedRead>| {
        let mut gaps = Vec::new();
        let mut all = Vec::new();
        for (_, g, s) in t {
            if let Some(g) = g {
                gaps.push((all.len(), g));
            }
            all.extend(s);
        }
        (gaps, all)
    };
    assert_eq!(flatten(trace(&mut replay)), flatten(bare));
    let meta = std::fs::read_to_string(metadata_path(&path)).unwrap();
    assert!(meta.contains(r#"{"type":"gap","before_sample":9010,"missing_samples":96000}"#));
    assert!(meta.contains(r#"{"type":"gap","before_sample":13109,"missing_samples":480}"#));
    assert!(meta.ends_with("{\"type\":\"end\",\"sample_count\":13109,\"reason\":\"eof\"}\n"));
}

#[test]
fn a_source_error_passes_through_and_the_prefix_is_finalized_as_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.cf32");
    let mut src = Scripted::new(vec![(None, block(0, 100))]);
    src.reads.push_back(Err("device lost".into()));
    let (mut rec, handle) = start_recording(Box::new(src), &path, SystemTime::now(), None).unwrap();
    let t = trace(&mut rec);
    assert_eq!(t.last().unwrap().0, Err("device lost".to_string()));
    drop(rec);
    match handle.finish(Duration::from_secs(5)) {
        RecordingOutcome::Finished {
            samples, reason, ..
        } => assert_eq!((samples, reason), (100, EndReason::Stopped)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_capture_finalizes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.cf32");
    let (rec, handle) = start_recording(
        Box::new(Scripted::new(Vec::new())),
        &path,
        SystemTime::now(),
        None,
    )
    .unwrap();
    drop(rec);
    assert!(matches!(
        handle.finish(Duration::from_secs(5)),
        RecordingOutcome::Finished { samples: 0, .. }
    ));
    assert_eq!(
        RecordedIqSource::open(&path)
            .unwrap()
            .summary()
            .sample_count,
        0
    );
}

#[test]
fn an_existing_destination_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.cf32");
    std::fs::write(&path, b"keep").unwrap();
    let err = start_recording(
        Box::new(Scripted::new(Vec::new())),
        &path,
        SystemTime::now(),
        None,
    )
    .err()
    .unwrap()
    .to_string();
    assert_eq!(
        err,
        format!(
            "IQ recording destination already exists: {}",
            path.display()
        )
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    assert!(!partial_metadata_path(&path).exists());

    // An existing metadata file or partial marker blocks it too, and the
    // payload this attempt would have created is not left behind.
    for marker in [metadata_path, partial_metadata_path] {
        let path = dir.path().join(format!(
            "m{}.cf32",
            marker(&path).display().to_string().len()
        ));
        std::fs::write(marker(&path), b"other").unwrap();
        assert!(start_recording(
            Box::new(Scripted::new(Vec::new())),
            &path,
            SystemTime::now(),
            None
        )
        .is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read(marker(&path)).unwrap(), b"other");
    }
}

#[test]
fn two_recorders_racing_for_one_destination_get_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.cf32");
    let (rec, handle) = start_recording(
        Box::new(Scripted::new(Vec::new())),
        &path,
        SystemTime::now(),
        None,
    )
    .unwrap();
    assert!(start_recording(
        Box::new(Scripted::new(Vec::new())),
        &path,
        SystemTime::now(),
        None
    )
    .is_err());
    drop(rec);
    assert!(matches!(
        handle.finish(Duration::from_secs(5)),
        RecordingOutcome::Finished { .. }
    ));
}
