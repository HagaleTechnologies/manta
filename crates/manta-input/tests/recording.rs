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
type TracedRead = (
    std::result::Result<usize, String>,
    Option<u64>,
    Vec<(u32, u32)>,
);

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
fn trace(src: &mut dyn IqSource) -> Vec<TracedRead> {
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
    // A multiple of 100 ns: Windows `SystemTime` has 100 ns resolution, so a
    // finer value would be truncated before it reaches the header.
    let start = SystemTime::UNIX_EPOCH + Duration::new(1_792_407_600, 123_400);
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
    assert_eq!(h.start_nanoseconds, 123_400);
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

/// Eight recorders started at once on one destination: exactly one wins,
/// every other fails as an existing destination, and the winner's pair is
/// complete.
#[test]
fn concurrent_recorders_for_one_destination_get_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.cf32");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let results: Vec<std::result::Result<RecordingOutcome, String>> = std::thread::scope(|s| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let path = &path;
                s.spawn(move || {
                    barrier.wait();
                    match start_recording(
                        Box::new(Scripted::new(vec![(None, block(0, 10))])),
                        path,
                        SystemTime::now(),
                        None,
                    ) {
                        Ok((mut rec, handle)) => {
                            let mut buf = vec![Complex32::new(0.0, 0.0); 16];
                            while rec.read(&mut buf).unwrap() > 0 {}
                            drop(rec);
                            Ok(handle.finish(Duration::from_secs(5)))
                        }
                        Err(e) => Err(e.to_string()),
                    }
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert!(
        matches!(winners[0], RecordingOutcome::Finished { samples: 10, .. }),
        "{results:?}"
    );
    for err in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(
            err.starts_with("IQ recording destination already exists: "),
            "{err}"
        );
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 80);
    assert!(metadata_path(&path).exists());
    assert!(!partial_metadata_path(&path).exists());
}

/// When the payload is created but its partial metadata cannot be (here a
/// dangling symlink, which the existence check does not see but exclusive
/// creation refuses), startup removes the payload it created and nothing
/// else.
#[cfg(unix)]
#[test]
fn a_failed_startup_removes_only_what_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("half.cf32");
    let target = dir.path().join("nowhere");
    std::os::unix::fs::symlink(&target, partial_metadata_path(&path)).unwrap();
    let err = start_recording(
        Box::new(Scripted::new(Vec::new())),
        &path,
        SystemTime::now(),
        None,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        err.starts_with("IQ recording destination already exists: "),
        "{err}"
    );
    assert!(!path.exists(), "the created payload is removed");
    assert!(std::fs::symlink_metadata(partial_metadata_path(&path)).is_ok());
    assert!(!target.exists(), "the symlink was not followed");
}

/// A source that ends after the run's stop flag is set (a live source
/// answering the stop) finalizes as `stopped`; one that ends on its own,
/// as `eof`.
#[test]
fn an_end_after_a_stop_request_is_a_stopped_recording() {
    let dir = tempfile::tempdir().unwrap();
    for (name, stopping, want) in [
        ("eof.cf32", false, EndReason::Eof),
        ("stopped.cf32", true, EndReason::Stopped),
    ] {
        let path = dir.path().join(name);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (rec, handle) = start_recording(
            Box::new(Scripted::new(vec![(None, block(0, 10))])),
            &path,
            SystemTime::now(),
            None,
        )
        .unwrap();
        let mut rec = rec.with_stop_flag(stop.clone());
        let mut buf = vec![Complex32::new(0.0, 0.0); 64];
        assert_eq!(rec.read(&mut buf).unwrap(), 10);
        stop.store(stopping, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(rec.read(&mut buf).unwrap(), 0);
        drop(rec);
        match handle.finish(Duration::from_secs(5)) {
            RecordingOutcome::Finished {
                samples: 10,
                reason,
                ..
            } => assert_eq!(reason, want, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// A live-like source: health counters, a confirmed-live flag, a spliced
/// loss, an acquisition offset and a gap before a read that spans several
/// writer blocks.
struct LiveLike {
    reads: VecDeque<(Option<u64>, Vec<Complex32>)>,
    pending: Option<u64>,
    missing: Option<u64>,
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    counters: std::sync::Arc<manta_input::InputHealthCounters>,
}

impl IqSource for LiveLike {
    fn sample_rate(&self) -> f64 {
        96_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        7_030_000.0
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let Some((gap, s)) = self.reads.pop_front() else {
            return Ok(0);
        };
        buf[..s.len()].copy_from_slice(&s);
        self.pending = gap;
        Ok(s.len())
    }
    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(self.live.clone())
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
    fn take_missing_samples(&mut self) -> Option<u64> {
        self.missing.take()
    }
    fn acquisition_start_offset(&self) -> Duration {
        Duration::from_millis(250)
    }
    fn health_counters(&self) -> Option<std::sync::Arc<manta_input::InputHealthCounters>> {
        Some(self.counters.clone())
    }
}

#[test]
fn the_recorder_forwards_every_live_hook_and_records_a_gap_before_a_multi_block_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.cf32");
    let live = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let counters = std::sync::Arc::new(manta_input::InputHealthCounters::new());
    let src = LiveLike {
        // 3 blocks of 4096 frames and part of a fourth, after a gap.
        reads: VecDeque::from(vec![
            (None, block(0, 100)),
            (Some(4_800), block(100, 13_000)),
        ]),
        pending: None,
        missing: Some(11),
        live: live.clone(),
        counters: counters.clone(),
    };
    let (mut rec, handle) = start_recording(Box::new(src), &path, SystemTime::now(), None).unwrap();
    let rec_live = rec
        .confirmed_live_handle()
        .expect("confirmed_live_handle forwarded");
    assert!(std::sync::Arc::ptr_eq(&rec_live, &live));
    let rec_counters = rec.health_counters().expect("health_counters forwarded");
    assert!(std::sync::Arc::ptr_eq(&rec_counters, &counters));
    assert_eq!(rec.acquisition_start_offset(), Duration::from_millis(250));
    assert_eq!(rec.take_missing_samples(), Some(11));
    assert_eq!(rec.take_missing_samples(), None);
    let t = trace(&mut rec);
    assert_eq!(t[0].0, Ok(100));
    assert_eq!(t[0].1, None);
    assert_eq!(t[1].0, Ok(13_000));
    assert_eq!(t[1].1, Some(4_800));
    assert_eq!(t[1].2, bits(&block(100, 13_000)));
    drop(rec);
    match handle.finish(Duration::from_secs(5)) {
        RecordingOutcome::Finished { samples, .. } => assert_eq!(samples, 13_100),
        other => panic!("{other:?}"),
    }
    let meta = std::fs::read_to_string(metadata_path(&path)).unwrap();
    let gaps: Vec<&str> = meta.lines().filter(|l| l.contains("\"gap\"")).collect();
    assert_eq!(
        gaps,
        [r#"{"type":"gap","before_sample":100,"missing_samples":4800}"#]
    );
    let mut replay = RecordedIqSource::open(&path).unwrap();
    let mut buf = vec![Complex32::new(0.0, 0.0); 20_000];
    let mut all = Vec::new();
    let mut gap_at = Vec::new();
    loop {
        let n = replay.read(&mut buf).unwrap();
        if let Some(g) = replay.take_discontinuity() {
            gap_at.push((all.len(), g));
        }
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    assert_eq!(gap_at, [(100, 4_800)]);
    assert_eq!(bits(&all), bits(&block(0, 13_100)));
}
