//! Recording the IQ a source delivers while it is decoded (MAN-309).
//!
//! `RecordingSource` wraps the source handed to the engine and passes every
//! read, error, property and discontinuity through unchanged. A copy of each
//! read goes, in fixed 4096-frame blocks, through a bounded non-blocking
//! queue to one writer thread per recording, which alone touches the files.
//! A full queue stops the recording (the file keeps a contiguous prefix) and
//! never makes the decoder wait; a write failure stops it too. Neither
//! becomes a source error. `RecordingHandle` reports the outcome.

use crate::recording_format::{
    metadata_path, partial_metadata_path, EndReason, MetadataRecord, RecordingHeader,
};
use crate::{InputHealthCounters, IqSource};
use anyhow::Result;
use num_complex::Complex32;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Frames per queued block.
pub const RECORDING_BLOCK_FRAMES: usize = 4096;
/// Payload bytes the queue may hold per recording.
pub const RECORDING_QUEUE_BYTES: usize = 16 * 1024 * 1024;
const QUEUE_BLOCKS: usize = RECORDING_QUEUE_BYTES / (RECORDING_BLOCK_FRAMES * 8);

/// Called once, from whichever thread sees it, when a recording stops early.
pub type RecordingNotice = Box<dyn Fn(&str) + Send + Sync>;

/// How a recording ended, as `RecordingHandle::finish` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingOutcome {
    /// The pair was published: payload plus final metadata.
    Finished {
        path: PathBuf,
        samples: u64,
        reason: EndReason,
    },
    /// The recording failed; the message says why.
    Failed(String),
    /// The writer did not finish within the deadline; completion is
    /// unconfirmed.
    TimedOut(PathBuf),
}

enum Msg {
    Block {
        gap: Option<u64>,
        samples: Vec<Complex32>,
    },
    TerminalGap(u64),
}

const REASON_STOPPED: u8 = 0;
const REASON_EOF: u8 = 1;
const REASON_BACKPRESSURE: u8 = 2;

struct Shared {
    sender: Mutex<Option<SyncSender<Msg>>>,
    reason: AtomicU8,
    cancelled: AtomicBool,
    notified: AtomicBool,
    notice: Option<RecordingNotice>,
}

impl Shared {
    fn close(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
    }

    /// Reports `msg` once across the producer and the writer.
    fn notify(&self, msg: &str) {
        if !self.notified.swap(true, Ordering::AcqRel) {
            if let Some(notice) = &self.notice {
                notice(msg);
            }
        }
    }
}

/// The wrapper the engine reads through; see the module docs.
pub struct RecordingSource<S: ?Sized> {
    inner: Box<S>,
    shared: Arc<Shared>,
    pending_gap: Option<u64>,
    stop: Option<Arc<AtomicBool>>,
}

/// The completion side of a recording, kept outside the source.
pub struct RecordingHandle {
    path: PathBuf,
    shared: Arc<Shared>,
    done: Receiver<RecordingOutcome>,
}

/// Starts recording `inner` to `path` (payload) and `PATH.meta.jsonl`, with
/// sample zero at `start`. Never overwrites: fails when the payload, the
/// metadata or its partial marker already exists. `notice` hears the one
/// early-stop warning, if any.
pub fn start_recording<S: IqSource + ?Sized>(
    inner: Box<S>,
    path: &Path,
    start: std::time::SystemTime,
    notice: Option<RecordingNotice>,
) -> Result<(RecordingSource<S>, RecordingHandle)> {
    let meta = metadata_path(path);
    let partial = partial_metadata_path(path);
    if path.exists() {
        anyhow::bail!(
            "IQ recording destination already exists: {}",
            path.display()
        );
    }
    if meta.exists() || partial.exists() {
        anyhow::bail!(
            "IQ recording destination already exists: {}",
            if meta.exists() { &meta } else { &partial }.display()
        );
    }
    let create = |p: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow::anyhow!("IQ recording destination already exists: {}", p.display())
                } else {
                    anyhow::Error::new(e).context(format!("create IQ recording {}", p.display()))
                }
            })
    };
    let payload = create(path)?;
    let meta_file = match create(&partial) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
    };
    let header = RecordingHeader::for_source(&*inner, start);
    let publish_path = path.to_path_buf();
    let publish: Publish = Box::new(move || publish_metadata(&publish_path));
    Ok(start_with_sinks(
        inner,
        path,
        header,
        Box::new(std::io::BufWriter::with_capacity(1 << 20, payload)),
        Box::new(std::io::BufWriter::new(meta_file)),
        publish,
        notice,
    ))
}

type Publish = Box<dyn FnOnce() -> std::io::Result<()> + Send>;

/// Renames `PATH.meta.jsonl.partial` to `PATH.meta.jsonl` without replacing
/// an existing file: a hard link fails on an existing target.
fn publish_metadata(path: &Path) -> std::io::Result<()> {
    let partial = partial_metadata_path(path);
    let meta = metadata_path(path);
    match std::fs::hard_link(&partial, &meta) {
        Ok(()) => std::fs::remove_file(&partial),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // A filesystem without hard links: check, then rename.
        Err(_) if !meta.exists() => std::fs::rename(&partial, &meta),
        Err(e) => Err(e),
    }
}

fn start_with_sinks<S: IqSource + ?Sized>(
    inner: Box<S>,
    path: &Path,
    header: RecordingHeader,
    payload: Box<dyn Write + Send>,
    meta: Box<dyn Write + Send>,
    publish: Publish,
    notice: Option<RecordingNotice>,
) -> (RecordingSource<S>, RecordingHandle) {
    let (tx, rx) = mpsc::sync_channel(QUEUE_BLOCKS);
    let (done_tx, done_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        sender: Mutex::new(Some(tx)),
        reason: AtomicU8::new(REASON_STOPPED),
        cancelled: AtomicBool::new(false),
        notified: AtomicBool::new(false),
        notice,
    });
    let worker_shared = shared.clone();
    let worker_path = path.to_path_buf();
    std::thread::Builder::new()
        .name("manta-iq-recorder".into())
        .spawn(move || {
            let outcome = write_recording(
                &worker_path,
                header,
                rx,
                payload,
                meta,
                publish,
                &worker_shared,
            );
            // After a timed-out `finish` the caller has already reported
            // the recording as unconfirmed; say nothing more.
            if let RecordingOutcome::Failed(msg) = &outcome {
                if !worker_shared.cancelled.load(Ordering::Acquire) {
                    worker_shared.notify(msg);
                }
            }
            let _ = done_tx.send(outcome);
        })
        .expect("spawn the IQ recording thread");
    (
        RecordingSource {
            inner,
            shared: shared.clone(),
            pending_gap: None,
            stop: None,
        },
        RecordingHandle {
            path: path.to_path_buf(),
            shared,
            done: done_rx,
        },
    )
}

fn write_recording(
    path: &Path,
    header: RecordingHeader,
    rx: Receiver<Msg>,
    mut payload: Box<dyn Write + Send>,
    mut meta: Box<dyn Write + Send>,
    publish: Publish,
    shared: &Shared,
) -> RecordingOutcome {
    let fail = |e: std::io::Error| {
        // Stop the producer: a failed recording stays failed.
        shared.close();
        RecordingOutcome::Failed(format!(
            "IQ recording stopped: write failed for {}: {e}; decoding continues",
            path.display()
        ))
    };
    if let Err(e) = meta.write_all(MetadataRecord::Header(header).to_line().as_bytes()) {
        return fail(e);
    }
    let mut samples: u64 = 0;
    // A gap is held until the next block or the end so that two gaps at one
    // position become one record.
    let mut pending: Option<u64> = None;
    let mut bytes = Vec::with_capacity(RECORDING_BLOCK_FRAMES * 8);
    let flush_gap =
        |pending: &mut Option<u64>, meta: &mut Box<dyn Write + Send>, at: u64| match pending.take()
        {
            Some(missing) => meta.write_all(
                MetadataRecord::Gap {
                    before_sample: at,
                    missing_samples: missing,
                }
                .to_line()
                .as_bytes(),
            ),
            None => Ok(()),
        };
    while let Ok(msg) = rx.recv() {
        let result = match msg {
            Msg::Block {
                gap,
                samples: block,
            } => {
                if let Some(g) = gap {
                    pending = Some(pending.unwrap_or(0).saturating_add(g));
                }
                bytes.clear();
                for s in &block {
                    bytes.extend_from_slice(&s.re.to_bits().to_le_bytes());
                    bytes.extend_from_slice(&s.im.to_bits().to_le_bytes());
                }
                flush_gap(&mut pending, &mut meta, samples).and_then(|()| {
                    payload.write_all(&bytes)?;
                    samples += block.len() as u64;
                    Ok(())
                })
            }
            Msg::TerminalGap(g) => {
                pending = Some(pending.unwrap_or(0).saturating_add(g));
                Ok(())
            }
        };
        if let Err(e) = result {
            return fail(e);
        }
    }
    let reason = match shared.reason.load(Ordering::Acquire) {
        REASON_EOF => EndReason::Eof,
        REASON_BACKPRESSURE => EndReason::Backpressure,
        _ => EndReason::Stopped,
    };
    let end = MetadataRecord::End {
        sample_count: samples,
        reason,
    };
    let finalize = flush_gap(&mut pending, &mut meta, samples)
        .and_then(|()| payload.flush())
        .and_then(|()| meta.write_all(end.to_line().as_bytes()))
        .and_then(|()| meta.flush());
    if let Err(e) = finalize {
        return fail(e);
    }
    drop(payload);
    drop(meta);
    if shared.cancelled.load(Ordering::Acquire) {
        return RecordingOutcome::Failed(format!(
            "IQ recording finalization timed out: {}; completion is unconfirmed",
            path.display()
        ));
    }
    if let Err(e) = publish() {
        return fail(e);
    }
    RecordingOutcome::Finished {
        path: path.to_path_buf(),
        samples,
        reason,
    }
}

impl<S: ?Sized> RecordingSource<S> {
    /// Finalizes as `stopped`, not `eof`, when the source ends after `stop`
    /// is set: a live source answers a stop request with EOF.
    pub fn with_stop_flag(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Copies `samples` (and the gap before them) into the queue; on a full
    /// queue, stops the recording at the last accepted block.
    fn enqueue(&self, mut gap: Option<u64>, samples: &[Complex32]) {
        let Ok(mut guard) = self.shared.sender.lock() else {
            return;
        };
        let Some(tx) = guard.as_ref() else {
            return;
        };
        let mut chunks = samples.chunks(RECORDING_BLOCK_FRAMES);
        loop {
            let msg = match (chunks.next(), gap.take()) {
                (Some(chunk), gap) => Msg::Block {
                    gap,
                    samples: chunk.to_vec(),
                },
                (None, Some(g)) if samples.is_empty() => Msg::TerminalGap(g),
                (None, _) => return,
            };
            match tx.try_send(msg) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.shared
                        .reason
                        .store(REASON_BACKPRESSURE, Ordering::Release);
                    guard.take();
                    drop(guard);
                    self.shared
                        .notify("IQ recording stopped: writer queue full; decoding continues");
                    return;
                }
                // The writer has stopped (and reported why).
                Err(TrySendError::Disconnected(_)) => {
                    guard.take();
                    return;
                }
            }
        }
    }
}

impl<S: IqSource + ?Sized> IqSource for RecordingSource<S> {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }

    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        // Consumed here, recorded with the block it precedes, and handed to
        // the engine unchanged.
        let gap = self.inner.take_discontinuity();
        self.pending_gap = gap;
        self.enqueue(gap, &buf[..n]);
        if n == 0 {
            // A live source answers a stop request with EOF: that is a
            // stopped session, not an input that ended.
            let stopping = self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::Relaxed));
            if !stopping && self.shared.reason.load(Ordering::Acquire) == REASON_STOPPED {
                self.shared.reason.store(REASON_EOF, Ordering::Release);
            }
            self.shared.close();
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending_gap.take()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

impl<S: ?Sized> Drop for RecordingSource<S> {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl RecordingHandle {
    /// The payload path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes the queue and waits at most `timeout` for the writer to
    /// publish. A timeout cancels publication if it has not begun.
    pub fn finish(self, timeout: Duration) -> RecordingOutcome {
        self.shared.close();
        match self.done.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(_) => {
                self.shared.cancelled.store(true, Ordering::Release);
                RecordingOutcome::TimedOut(self.path.clone())
            }
        }
    }
}

impl Drop for RecordingHandle {
    fn drop(&mut self) {
        self.shared.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Endless constant source, `n` frames per read.
    struct Endless;

    impl IqSource for Endless {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            buf.fill(Complex32::new(0.25, -0.25));
            Ok(buf.len())
        }
    }

    /// A sink that waits for `gate` before its first write, then fails or
    /// counts (and keeps) bytes; `fail_flush` fails every flush instead.
    struct Sink {
        gate: Option<std::sync::mpsc::Receiver<()>>,
        fail: bool,
        fail_flush: bool,
        bytes: Arc<AtomicUsize>,
        kept: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(gate) = self.gate.take() {
                let _ = gate.recv();
            }
            if self.fail {
                return Err(std::io::Error::other("disk on fire"));
            }
            self.bytes.fetch_add(buf.len(), Ordering::Relaxed);
            self.kept.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                return Err(std::io::Error::other("flush lost"));
            }
            Ok(())
        }
    }

    fn sink(gate: Option<std::sync::mpsc::Receiver<()>>, fail: bool) -> (Sink, Arc<AtomicUsize>) {
        let (sink, bytes, _) = kept_sink(gate, fail, false);
        (sink, bytes)
    }

    #[allow(clippy::type_complexity)]
    fn kept_sink(
        gate: Option<std::sync::mpsc::Receiver<()>>,
        fail: bool,
        fail_flush: bool,
    ) -> (Sink, Arc<AtomicUsize>, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(AtomicUsize::new(0));
        let kept = Arc::new(Mutex::new(Vec::new()));
        (
            Sink {
                gate,
                fail,
                fail_flush,
                bytes: bytes.clone(),
                kept: kept.clone(),
            },
            bytes,
            kept,
        )
    }

    /// Endless constant source whose read number `gap_after` (1-based) is
    /// followed by a `missing`-sample discontinuity.
    struct GapOnce {
        reads: usize,
        gap_after: usize,
        missing: u64,
        pending: Option<u64>,
    }

    impl IqSource for GapOnce {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            buf.fill(Complex32::new(0.25, -0.25));
            self.reads += 1;
            if self.reads == self.gap_after {
                self.pending = Some(self.missing);
            }
            Ok(buf.len())
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.pending.take()
        }
    }

    struct Notices(Arc<Mutex<Vec<String>>>);

    impl Notices {
        fn new() -> (Self, RecordingNotice) {
            let log = Arc::new(Mutex::new(Vec::new()));
            let sink = log.clone();
            (
                Notices(log),
                Box::new(move |m: &str| sink.lock().unwrap().push(m.to_string())),
            )
        }
        fn get(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    fn start(
        payload: Sink,
        meta: Sink,
        published: Arc<AtomicBool>,
        notice: RecordingNotice,
    ) -> (RecordingSource<Endless>, RecordingHandle) {
        let header = RecordingHeader::for_source(&Endless, std::time::UNIX_EPOCH);
        start_with_sinks(
            Box::new(Endless),
            Path::new("test.cf32"),
            header,
            Box::new(payload),
            Box::new(meta),
            Box::new(move || {
                published.store(true, Ordering::Release);
                Ok(())
            }),
            Some(notice),
        )
    }

    #[test]
    fn a_full_queue_stops_recording_without_blocking_reads() {
        let (release, gate) = std::sync::mpsc::channel();
        let (payload, payload_bytes) = sink(None, false);
        // The metadata header write blocks the writer before any block.
        let (meta, _) = sink(Some(gate), false);
        let published = Arc::new(AtomicBool::new(false));
        let (notices, notice) = Notices::new();
        let (mut rec, handle) = start(payload, meta, published.clone(), notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); RECORDING_BLOCK_FRAMES * 100];
        // 6 reads x 100 blocks overflow the 512-block queue; none waits.
        for _ in 0..6 {
            assert_eq!(rec.read(&mut buf).unwrap(), buf.len());
        }
        assert_eq!(
            notices.get(),
            vec!["IQ recording stopped: writer queue full; decoding continues".to_string()]
        );
        // Reads keep working after the recording stopped.
        assert_eq!(rec.read(&mut buf).unwrap(), buf.len());
        release.send(()).unwrap();
        drop(rec);
        match handle.finish(Duration::from_secs(10)) {
            RecordingOutcome::Finished {
                samples, reason, ..
            } => {
                assert_eq!(reason, EndReason::Backpressure);
                // Exactly the accepted contiguous prefix: whole blocks.
                assert_eq!(samples, (QUEUE_BLOCKS * RECORDING_BLOCK_FRAMES) as u64);
                assert_eq!(payload_bytes.load(Ordering::Relaxed), samples as usize * 8);
            }
            other => panic!("{other:?}"),
        }
        assert!(published.load(Ordering::Acquire));
        // Queued payload never exceeded the bound.
        const { assert!(QUEUE_BLOCKS * RECORDING_BLOCK_FRAMES * 8 <= RECORDING_QUEUE_BYTES) };
    }

    #[test]
    fn a_write_failure_is_reported_once_and_reading_continues() {
        for fail_payload in [true, false] {
            let (payload, _) = sink(None, fail_payload);
            let (meta, _) = sink(None, !fail_payload);
            let published = Arc::new(AtomicBool::new(false));
            let (notices, notice) = Notices::new();
            let (mut rec, handle) = start(payload, meta, published.clone(), notice);
            let mut buf = vec![Complex32::new(0.0, 0.0); 5000];
            for _ in 0..50 {
                assert_eq!(rec.read(&mut buf).unwrap(), 5000);
            }
            drop(rec);
            let outcome = handle.finish(Duration::from_secs(10));
            assert_eq!(
                outcome,
                RecordingOutcome::Failed(
                    "IQ recording stopped: write failed for test.cf32: disk on fire; decoding \
                     continues"
                        .into()
                )
            );
            assert_eq!(notices.get().len(), 1, "{:?}", notices.get());
            assert!(!published.load(Ordering::Acquire));
        }
    }

    #[test]
    fn a_publication_failure_is_a_failed_recording() {
        let (payload, _) = sink(None, false);
        let (meta, _) = sink(None, false);
        let header = RecordingHeader::for_source(&Endless, std::time::UNIX_EPOCH);
        let (rec, handle) = start_with_sinks(
            Box::new(Endless),
            Path::new("p.cf32"),
            header,
            Box::new(payload),
            Box::new(meta),
            Box::new(|| Err(std::io::Error::other("exists"))),
            None,
        );
        drop(rec);
        assert!(matches!(
            handle.finish(Duration::from_secs(10)),
            RecordingOutcome::Failed(_)
        ));
    }

    #[test]
    fn finish_honours_its_deadline_and_cancels_publication() {
        let (release, gate) = std::sync::mpsc::channel();
        let (payload, _) = sink(None, false);
        let (meta, _) = sink(Some(gate), false);
        let published = Arc::new(AtomicBool::new(false));
        let (notices, notice) = Notices::new();
        let (rec, handle) = start(payload, meta, published.clone(), notice);
        drop(rec);
        let started = std::time::Instant::now();
        assert_eq!(
            handle.finish(Duration::from_millis(50)),
            RecordingOutcome::TimedOut(PathBuf::from("test.cf32"))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // Unblock the writer: it must neither publish nor report later.
        release.send(()).unwrap();
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!published.load(Ordering::Acquire));
        assert!(notices.get().is_empty());
    }

    #[test]
    fn a_flush_failure_is_a_failed_recording() {
        for flush_payload in [true, false] {
            let (payload, _, _) = kept_sink(None, false, flush_payload);
            let (meta, _, _) = kept_sink(None, false, !flush_payload);
            let published = Arc::new(AtomicBool::new(false));
            let (notices, notice) = Notices::new();
            let (mut rec, handle) = start(payload, meta, published.clone(), notice);
            let mut buf = vec![Complex32::new(0.0, 0.0); 5000];
            for _ in 0..10 {
                assert_eq!(rec.read(&mut buf).unwrap(), 5000);
            }
            drop(rec);
            match handle.finish(Duration::from_secs(10)) {
                RecordingOutcome::Failed(msg) => {
                    assert!(msg.contains("flush lost"), "{msg}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(notices.get().len(), 1, "{:?}", notices.get());
            assert!(!published.load(Ordering::Acquire));
        }
    }

    #[test]
    fn a_gap_refused_by_a_full_queue_describes_no_unrecorded_samples() {
        let (release, gate) = std::sync::mpsc::channel();
        let (payload, payload_bytes, _) = kept_sink(None, false, false);
        // The metadata header write holds the writer before any block.
        let (meta, _, meta_kept) = kept_sink(Some(gate), false, false);
        let published = Arc::new(AtomicBool::new(false));
        let (notices, notice) = Notices::new();
        let src = GapOnce {
            reads: 0,
            gap_after: 2,
            missing: 1000,
            pending: None,
        };
        let header = RecordingHeader::for_source(&src, std::time::UNIX_EPOCH);
        let (mut rec, handle) = start_with_sinks(
            Box::new(src),
            Path::new("gap.cf32"),
            header,
            Box::new(payload),
            Box::new(meta),
            Box::new(move || {
                published.store(true, Ordering::Release);
                Ok(())
            }),
            Some(notice),
        );
        // Read 1 fills the queue exactly; read 2 carries the gap and is
        // refused, so the recording ends before it.
        let mut full = vec![Complex32::new(0.0, 0.0); QUEUE_BLOCKS * RECORDING_BLOCK_FRAMES];
        assert_eq!(rec.read(&mut full).unwrap(), full.len());
        assert_eq!(rec.take_discontinuity(), None);
        let mut one = vec![Complex32::new(0.0, 0.0); RECORDING_BLOCK_FRAMES];
        assert_eq!(rec.read(&mut one).unwrap(), one.len());
        // The engine still sees the gap.
        assert_eq!(rec.take_discontinuity(), Some(1000));
        assert_eq!(notices.get().len(), 1, "{:?}", notices.get());
        release.send(()).unwrap();
        drop(rec);
        match handle.finish(Duration::from_secs(10)) {
            RecordingOutcome::Finished {
                samples, reason, ..
            } => {
                assert_eq!(reason, EndReason::Backpressure);
                assert_eq!(samples, (QUEUE_BLOCKS * RECORDING_BLOCK_FRAMES) as u64);
                assert_eq!(payload_bytes.load(Ordering::Relaxed), samples as usize * 8);
            }
            other => panic!("{other:?}"),
        }
        let meta = String::from_utf8(meta_kept.lock().unwrap().clone()).unwrap();
        assert!(!meta.contains("\"gap\""), "{meta}");
        assert!(meta.contains("\"backpressure\""), "{meta}");
    }
}
