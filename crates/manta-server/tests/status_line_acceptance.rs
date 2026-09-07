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

#[tokio::test]
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
    let handle = status::spawn_status_line(metrics.clone(), Duration::from_millis(50), 1, rx)
        .expect("a non-zero interval must spawn the task");

    tokio::time::sleep(Duration::from_millis(180)).await;
    handle.abort();

    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("manta status:"))
        .collect();
    assert!(
        !lines.is_empty(),
        "no status line was emitted at all; captured: {text:?}"
    );
    let first = lines[0];
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
        lines.len() <= 4,
        "expected at most one line per 50 ms interval over 180 ms, got {}: {text:?}",
        lines.len()
    );
}
