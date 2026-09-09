//! MAN-122 scenario 2 acceptance.
//!
//! ONE test per file on purpose: it installs a process-global tracing
//! subscriber to capture what the status task actually emits, and a global
//! can only be installed once. `cargo test` gives each integration-test
//! FILE its own binary/process, so this file owns one.

use manta_server::metrics::Metrics;
use manta_server::status;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Minimal `MakeWriter` that appends every formatted event into a shared
/// buffer the test can read back. `Arc<Mutex<Vec<u8>>>` alone does NOT
/// implement `MakeWriter` (the blanket `Arc<W>` impl needs `&W: Write`).
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Every `manta status:` line captured so far, as owned strings: the buffer
/// is re-read between clock advances, so nothing may borrow from an
/// intermediate snapshot of it.
fn status_lines(captured: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    text.lines()
        .filter(|l| l.contains("manta status:"))
        .map(str::to_owned)
        .collect()
}

/// `start_paused` plus explicit advances, rather than a wall-clock sleep
/// long enough to "probably" cover a few intervals (MAN-122 review round
/// 3). On a current-thread executor descheduled long enough for both the
/// task's 50 ms status timer and a parent wall-clock timer to come ready,
/// the parent can resume first and observe an empty capture -- a
/// scheduler-speed flake in a test that gates merging on two platforms.
/// Here the clock only moves when this test moves it, and the test
/// synchronizes on the first captured event instead of on elapsed time.
#[tokio::test(start_paused = true)]
async fn the_periodic_status_line_reports_tracks_spot_rate_clients_and_uplink_state() {
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    tracing_subscriber::fmt()
        .with_writer(Capture(captured.clone()))
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .init();

    let metrics = Arc::new(Metrics::new());
    metrics.set_active_tracks(3);
    metrics.inc_telnet_clients();
    metrics.inc_json_clients();
    metrics.mark_uplink_connected();
    for _ in 0..5 {
        metrics.record_spot();
    }

    let (_tx, rx) = tokio::sync::watch::channel(false);
    let interval = Duration::from_millis(50);
    let handle = status::spawn_status_line(metrics.clone(), interval, 1, rx)
        .expect("a non-zero interval must spawn the task");

    // Advance one interval at a time until the first line is actually in the
    // buffer: `advance` wakes the task, the `yield_now` lets it run through
    // to its `tracing::info!` before this test reads the buffer again.
    // Bounded, so a task that never emits fails with the capture in hand
    // instead of hanging the suite.
    let mut intervals = 0;
    while status_lines(&captured).is_empty() {
        assert!(
            intervals < 20,
            "no status line after {intervals} advances of the {interval:?} interval; \
             captured: {:?}",
            String::from_utf8(captured.lock().unwrap().clone()).unwrap()
        );
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
        intervals += 1;
    }

    // Two further intervals, so the rate-limit assertion below has more than
    // one interval of room to catch a task that emits per poll.
    for _ in 0..2 {
        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;
        intervals += 1;
    }
    handle.abort();

    let lines = status_lines(&captured);
    let first = &lines[0];
    for needle in [
        "tracks=3",
        "spots_per_min=",
        "clients=2",
        "uplink=connected",
    ] {
        assert!(first.contains(needle), "{needle:?} missing: {first:?}");
    }
    // Rate limiting: one line per interval, not a stream.
    assert!(
        lines.len() <= intervals,
        "expected at most one line per {interval:?} interval over {intervals} intervals, \
         got {}: {lines:?}",
        lines.len()
    );
}
