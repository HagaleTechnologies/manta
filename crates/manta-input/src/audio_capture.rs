//! MAN-530: the live rig-audio capture transport behind
//! `AudioIqSource::from_device`.
//!
//! A bounded single-producer/single-consumer ring of capture-index-tagged
//! frames. The cpal callback (the producer) accepts the prefix of each
//! callback's data that fits and rejects the rest, coppa's drop-newest
//! policy, but every frame it is handed advances the capture index,
//! rejected ones included. The reader therefore sees an exact hole as a
//! jump in the index of the next queued frame, and stops a read before it,
//! so no returned buffer straddles lost audio.
//!
//! Why tags and not coppa's overflow count: two independent snapshots of a
//! cumulative count and of ring occupancy cannot locate a hole while the
//! callback keeps publishing. Measured on the pinned coppa ring: after a
//! parked consumer read 2048 pre-loss frames and 2048 post-loss frames
//! arrived, the occupancy-derived boundary was 10240 frames in, the true
//! one 8192 (docs/DECISIONS/2026-11-09-man530-audio-ring-overrun-visibility.md).
//!
//! The callback touches only the ring and two atomic counters: no lock,
//! allocation, logging, sleep or channel send. Device lookup, stream
//! build/play and stream destruction all run on one owner thread, so no
//! cpal object ever crosses a thread and no `unsafe impl Send` is needed.

use crate::InputHealthCounters;
use anyhow::{anyhow, Result};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Production capture-ring capacity: 8192 frames, ~170 ms at 48 kHz, the
/// size coppa's `CpalSource` used.
pub(crate) const CAPTURE_RING_FRAMES: usize = 8192;

/// One captured mono sample and its position in the capture stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaggedFrame {
    sample: f32,
    capture_index: u64,
}

/// The callback side of the ring. Owned by exactly one cpal callback.
pub(crate) struct CaptureProducer {
    producer: rtrb::Producer<TaggedFrame>,
    /// Capture index of the next frame the callback is handed.
    next_index: u64,
    /// Inside a loss episode: the last frame handed over was rejected.
    in_loss: bool,
    health: Arc<InputHealthCounters>,
}

/// The reader side of the ring.
pub(crate) struct CaptureConsumer {
    consumer: rtrb::Consumer<TaggedFrame>,
    /// Capture index the next returned frame must carry to be contiguous.
    expected: u64,
}

/// What one `CaptureConsumer::read_run` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CaptureRead {
    /// Frames copied, all contiguous in capture order.
    pub(crate) got: usize,
    /// Frames lost immediately before the first copied frame (0 = none).
    pub(crate) missing: u64,
}

/// A tagged ring of `capacity` frames whose first frame carries
/// `start_index`. Production starts at 0; tests start near `u64::MAX` to
/// exercise wrap.
pub(crate) fn capture_ring(
    capacity: usize,
    start_index: u64,
    health: Arc<InputHealthCounters>,
) -> (CaptureProducer, CaptureConsumer) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    (
        CaptureProducer {
            producer,
            next_index: start_index,
            in_loss: false,
            health,
        },
        CaptureConsumer {
            consumer,
            expected: start_index,
        },
    )
}

impl CaptureProducer {
    /// One callback's worth of capture. Accepts the prefix that fits in the
    /// free slots counted once, rejects the tail, and accounts the rejected
    /// frames exactly: `queue_dropped_samples` by their number and
    /// `gaps_detected` once per contiguous loss episode. Lock- and
    /// allocation-free: this runs in the audio callback.
    pub(crate) fn write(&mut self, data: &[f32]) {
        if data.is_empty() {
            return;
        }
        let accept = self.producer.slots().min(data.len());
        for &sample in &data[..accept] {
            // Cannot fail: this sole producer counted `accept` free slots,
            // and the consumer only ever frees more.
            let _ = self.producer.push(TaggedFrame {
                sample,
                capture_index: self.next_index,
            });
            self.next_index = self.next_index.wrapping_add(1);
        }
        if accept > 0 {
            // An accepted frame ends any loss episode in progress.
            self.in_loss = false;
        }
        let rejected = (data.len() - accept) as u64;
        if rejected > 0 {
            if !self.in_loss {
                self.health.record_gap();
                self.in_loss = true;
            }
            self.health.record_queue_dropped(rejected);
            self.next_index = self.next_index.wrapping_add(rejected);
        }
    }
}

impl CaptureConsumer {
    /// Copy the next contiguous run of queued frames into `out`, at most
    /// `out.len()`. Stops before the first frame whose capture index jumps,
    /// so a returned run lies entirely on one side of a hole; when the run
    /// itself begins after a hole, `missing` is the hole's exact length.
    /// Locates holes only from tags, never from counters or occupancy.
    pub(crate) fn read_run(&mut self, out: &mut [f32]) -> CaptureRead {
        let available = self.consumer.slots().min(out.len());
        if available == 0 {
            return CaptureRead::default();
        }
        let Ok(chunk) = self.consumer.read_chunk(available) else {
            return CaptureRead::default();
        };
        let (head, tail) = chunk.as_slices();
        let Some(first) = head.first().or_else(|| tail.first()) else {
            return CaptureRead::default();
        };
        let missing = first.capture_index.wrapping_sub(self.expected);
        let mut expected = first.capture_index;
        let mut got = 0;
        for frame in head.iter().chain(tail) {
            if frame.capture_index != expected {
                break;
            }
            out[got] = frame.sample;
            got += 1;
            expected = expected.wrapping_add(1);
        }
        chunk.commit(got);
        self.expected = expected;
        CaptureRead { got, missing }
    }
}

/// Where a live audio read comes from: the tagged production ring, or (in
/// tests) an untagged scripted source that never reports a hole.
pub(crate) trait CaptureRing: Send {
    fn read_run(&mut self, out: &mut [f32]) -> Result<CaptureRead>;
}

impl CaptureRing for CaptureConsumer {
    fn read_run(&mut self, out: &mut [f32]) -> Result<CaptureRead> {
        Ok(CaptureConsumer::read_run(self, out))
    }
}

/// Keeps a live capture stream alive. Dropping it signals the owner
/// thread, which destroys the cpal stream on the thread that built it,
/// and joins that thread (never from inside the callback).
pub(crate) struct CaptureStream {
    shutdown: Option<mpsc::Sender<()>>,
    owner: Option<JoinHandle<()>>,
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        // Closing the channel wakes the owner's `recv`.
        drop(self.shutdown.take());
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

/// What the owner thread hands back once its stream is playing.
pub(crate) struct OpenedCapture {
    pub(crate) consumer: CaptureConsumer,
    pub(crate) stream: CaptureStream,
    /// The device's name as `manta devices` spells it (quoted).
    pub(crate) label: String,
    /// `from_device`'s runtime up to stream play (MAN-259).
    pub(crate) acquisition_start_offset: Duration,
}

/// Request/answer types crossing to and from the owner thread: only the
/// input name, the health Arc, the consumer and a startup result.
type Started = Result<(CaptureConsumer, String, Duration)>;

/// Open a live capture stream on a dedicated owner thread. `open` runs on
/// that thread: it looks the device up, builds and plays the stream with
/// the producer it is given, and returns the stream (kept alive there until
/// shutdown), the device label and the acquisition offset. Startup waits
/// for success or `open`'s fully contextualized error; a panicking owner
/// closes the handshake and becomes an error, never a hang.
pub(crate) fn spawn_owner<S, F>(
    capacity: usize,
    health: Arc<InputHealthCounters>,
    entered: Instant,
    open: F,
) -> Result<OpenedCapture>
where
    S: 'static,
    F: FnOnce(CaptureProducer, Instant) -> Result<(S, String, Duration)> + Send + 'static,
{
    let (started_tx, started_rx) = mpsc::sync_channel::<Started>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let owner = std::thread::Builder::new()
        .name("manta-audio-capture".to_string())
        .spawn(move || {
            let (producer, consumer) = capture_ring(capacity, 0, health);
            match open(producer, entered) {
                Ok((stream, label, offset)) => {
                    if started_tx.send(Ok((consumer, label, offset))).is_err() {
                        return;
                    }
                    // Park until the source drops its sender, then destroy
                    // the stream here, on the thread that built it.
                    let _ = shutdown_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = started_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| anyhow!("could not start the audio capture thread: {e}"))?;
    let mut stream = CaptureStream {
        shutdown: Some(shutdown_tx),
        owner: Some(owner),
    };
    match started_rx.recv() {
        Ok(Ok((consumer, label, acquisition_start_offset))) => Ok(OpenedCapture {
            consumer,
            stream,
            label,
            acquisition_start_offset,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // The owner died before answering: join it, then report.
            if let Some(owner) = stream.owner.take() {
                let _ = owner.join();
            }
            Err(anyhow!(
                "the audio capture thread exited before the input started"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(
        capacity: usize,
        start: u64,
    ) -> (CaptureProducer, CaptureConsumer, Arc<InputHealthCounters>) {
        let health = Arc::new(InputHealthCounters::new());
        let (p, c) = capture_ring(capacity, start, Arc::clone(&health));
        (p, c, health)
    }

    /// Frames whose value is their offset in the generated stream.
    fn frames(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|i| i as f32).collect()
    }

    fn drain(c: &mut CaptureConsumer, want: usize) -> (Vec<f32>, u64) {
        let mut out = vec![0.0; want];
        let r = c.read_run(&mut out);
        out.truncate(r.got);
        (out, r.missing)
    }

    #[test]
    fn no_loss_returns_every_frame_once_with_no_hole() {
        let (mut p, mut c, health) = ring(64, 0);
        p.write(&frames(0, 40));
        let (got, missing) = drain(&mut c, 64);
        assert_eq!(got, frames(0, 40));
        assert_eq!(missing, 0);
        assert_eq!(health.queue_dropped_samples(), 0);
        assert_eq!(health.gaps_detected(), 0);
    }

    #[test]
    fn an_empty_callback_changes_nothing() {
        let (mut p, mut c, health) = ring(4, 0);
        p.write(&frames(0, 4));
        p.write(&[]);
        assert_eq!(health.queue_dropped_samples(), 0);
        assert_eq!(health.gaps_detected(), 0);
        assert_eq!(drain(&mut c, 8), (frames(0, 4), 0));
    }

    #[test]
    fn a_full_ring_rejects_a_whole_callback_as_one_gap() {
        let (mut p, mut c, health) = ring(4, 0);
        p.write(&frames(0, 4));
        p.write(&frames(4, 3));
        assert_eq!(health.queue_dropped_samples(), 3);
        assert_eq!(health.gaps_detected(), 1);
        assert_eq!(drain(&mut c, 8), (frames(0, 4), 0));
        p.write(&frames(7, 2));
        assert_eq!(drain(&mut c, 8), (frames(7, 2), 3));
    }

    #[test]
    fn a_partial_callback_accepts_the_prefix_and_counts_the_suffix() {
        let (mut p, mut c, health) = ring(4, 0);
        p.write(&frames(0, 6));
        assert_eq!(health.queue_dropped_samples(), 2);
        assert_eq!(health.gaps_detected(), 1);
        assert_eq!(drain(&mut c, 8), (frames(0, 4), 0));
        p.write(&frames(6, 1));
        assert_eq!(drain(&mut c, 8), (frames(6, 1), 2));
    }

    #[test]
    fn adjacent_dropping_callbacks_are_one_loss_episode() {
        let (mut p, _c, health) = ring(2, 0);
        p.write(&frames(0, 2));
        for i in 0..5 {
            p.write(&frames(2 + i * 3, 3));
        }
        assert_eq!(health.queue_dropped_samples(), 15);
        assert_eq!(health.gaps_detected(), 1);
    }

    #[test]
    fn losses_separated_by_an_accepted_frame_are_two_episodes() {
        let (mut p, mut c, health) = ring(2, 0);
        p.write(&frames(0, 3)); // 2 accepted, 1 lost
        assert_eq!(drain(&mut c, 1), (frames(0, 1), 0));
        p.write(&frames(3, 3)); // 1 accepted, 2 lost: a new episode
        assert_eq!(health.queue_dropped_samples(), 3);
        assert_eq!(health.gaps_detected(), 2);
        // Runs: [1] | hole of 1 | [3] | hole of 2.
        assert_eq!(drain(&mut c, 8), (frames(1, 1), 0));
        assert_eq!(drain(&mut c, 8), (frames(3, 1), 1));
        p.write(&frames(6, 1));
        assert_eq!(drain(&mut c, 8), (frames(6, 1), 2));
    }

    #[test]
    fn a_parked_production_ring_retains_8192_and_counts_15808() {
        let (mut p, mut c, health) = ring(CAPTURE_RING_FRAMES, 0);
        // 500 ms at 48 kHz in 10 ms callbacks, consumer parked.
        for i in 0..50 {
            p.write(&frames(i * 480, 480));
        }
        assert_eq!(health.queue_dropped_samples(), 15_808);
        assert_eq!(health.gaps_detected(), 1);
        let (pre, missing) = drain(&mut c, 24_000);
        assert_eq!(pre.len(), 8192);
        assert_eq!(missing, 0);
        // Capture resumes: the next accepted frame is index 24000.
        p.write(&frames(24_000, 480));
        let (post, missing) = drain(&mut c, 24_000);
        assert_eq!(missing, 15_808);
        assert_eq!(post.first().copied(), Some(24_000.0));
        assert_eq!(post.len(), 480);
    }

    #[test]
    fn the_boundary_stays_at_8192_while_the_producer_publishes_mid_read() {
        // The measured counterexample: read 2048 pre-loss frames, then
        // 2048 post-loss frames arrive before the next read. An occupancy
        // snapshot puts the hole at 10240; the tags keep it at 8192.
        let (mut p, mut c, health) = ring(CAPTURE_RING_FRAMES, 0);
        for i in 0..50 {
            p.write(&frames(i * 480, 480));
        }
        let (first, _) = drain(&mut c, 2048);
        assert_eq!(first, frames(0, 2048));
        p.write(&frames(24_000, 2048));
        assert_eq!(health.queue_dropped_samples(), 15_808);
        // Counters are visible before delivery reaches the hole.
        let (pre, missing) = drain(&mut c, 8192);
        assert_eq!(pre, frames(2048, 6144));
        assert_eq!(missing, 0);
        let (post, missing) = drain(&mut c, 8192);
        assert_eq!(post, frames(24_000, 2048));
        assert_eq!(missing, 15_808);
    }

    #[test]
    fn frames_freed_between_callbacks_are_accepted_without_a_hole() {
        let (mut p, mut c, health) = ring(4, 0);
        p.write(&frames(0, 4));
        assert_eq!(drain(&mut c, 2), (frames(0, 2), 0));
        p.write(&frames(4, 2));
        assert_eq!(health.queue_dropped_samples(), 0);
        assert_eq!(drain(&mut c, 8), (frames(2, 4), 0));
    }

    #[test]
    fn several_holes_never_share_a_run_and_each_is_exact() {
        let (mut p, mut c, health) = ring(3, 0);
        p.write(&frames(0, 5)); // [0,1,2] lost 3,4
        assert_eq!(drain(&mut c, 3), (frames(0, 3), 0));
        p.write(&frames(5, 4)); // [5,6,7] lost 8
        assert_eq!(drain(&mut c, 3), (frames(5, 3), 2));
        p.write(&frames(9, 1)); // [9]
        assert_eq!(drain(&mut c, 3), (frames(9, 1), 1));
        assert_eq!(health.queue_dropped_samples(), 3);
        assert_eq!(health.gaps_detected(), 2);
    }

    #[test]
    fn capture_index_wraps_without_a_false_hole() {
        let (mut p, mut c, health) = ring(4, u64::MAX - 1);
        p.write(&frames(0, 6)); // indices MAX-1, MAX, 0, 1 accepted; 2, 3 lost
        assert_eq!(drain(&mut c, 8), (frames(0, 4), 0));
        p.write(&frames(6, 2));
        assert_eq!(drain(&mut c, 8), (frames(6, 2), 2));
        assert_eq!(health.queue_dropped_samples(), 2);
    }

    #[test]
    fn an_empty_request_consumes_nothing() {
        let (mut p, mut c, _health) = ring(4, 0);
        p.write(&frames(0, 6));
        assert_eq!(drain(&mut c, 0), (vec![], 0));
        assert_eq!(drain(&mut c, 8), (frames(0, 4), 0));
    }

    #[test]
    fn a_loss_is_counted_even_if_capture_never_resumes() {
        let (mut p, _c, health) = ring(4, 0);
        p.write(&frames(0, 10));
        drop(p);
        assert_eq!(health.queue_dropped_samples(), 6);
        assert_eq!(health.gaps_detected(), 1);
    }

    #[test]
    fn the_owner_thread_reports_an_open_error_and_exits() {
        let health = Arc::new(InputHealthCounters::new());
        let err = spawn_owner::<(), _>(4, health, Instant::now(), |_p, _t| {
            Err(anyhow!("audio input \"X\" did not open"))
        })
        .err()
        .expect("open failed");
        assert!(err.to_string().contains("did not open"));
    }

    #[test]
    fn a_panicking_owner_becomes_an_error_not_a_hang() {
        let health = Arc::new(InputHealthCounters::new());
        let err = spawn_owner::<(), _>(
            4,
            health,
            Instant::now(),
            |_p, _t| -> Result<((), String, Duration)> { panic!("owner died") },
        )
        .err()
        .expect("owner panicked");
        assert!(err.to_string().contains("exited before the input started"));
    }

    #[test]
    fn the_owner_keeps_the_stream_until_the_source_drops_it() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Flag(Arc::clone(&dropped));
        let health = Arc::new(InputHealthCounters::new());
        let opened = spawn_owner(4, health, Instant::now(), move |mut p, entered| {
            p.write(&[0.5, 0.25]);
            Ok((flag, "\"Fake\"".to_string(), entered.elapsed()))
        })
        .expect("opened");
        assert_eq!(opened.label, "\"Fake\"");
        let OpenedCapture {
            mut consumer,
            stream,
            ..
        } = opened;
        let mut out = [0.0; 4];
        assert_eq!(consumer.read_run(&mut out).got, 2);
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        drop(stream);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}
