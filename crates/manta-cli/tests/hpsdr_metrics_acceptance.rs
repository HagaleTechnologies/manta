//! MAN-56 acceptance:
//!   Given manta is running with an HPSDR/Hermes input source
//!   When packets are dropped, gapped, or discarded as malformed
//!   Then those counts are visible on the Prometheus /metrics endpoint
//!     without needing to attach a debugger or read gap_stats() from code
//!
//! No hardware is used: a fake HPSDR device on a loopback UDP socket drips
//! malformed datagrams at a real `manta listen` child process, and this
//! test scrapes the real `/metrics` HTTP endpoint. Feasible without a
//! valid Metis packet (which would mean duplicating `manta-input`'s
//! private protocol-framing test helpers): `MAX_CONSECUTIVE_MALFORMED` is
//! 10 000 and `MAX_CONSECUTIVE_TIMEOUTS` is 40 * 250 ms
//! (`crates/manta-input/src/hpsdr.rs`), and a `recv()` that returns a
//! malformed datagram is not a timeout, so a drip of one garbage datagram
//! every 100 ms keeps the daemon alive for tens of minutes -- orders of
//! magnitude more headroom than this test needs.
#![cfg(feature = "hpsdr")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// A Metis packet's wire length (`manta_input::hpsdr`'s
/// `METIS_HEADER_LEN` + `USB_FRAMES_PER_PACKET` * `USB_FRAME_LEN` = 8 + 2 *
/// 512), mirrored here as a literal rather than imported: those constants
/// are private to `manta-input`, and this test only needs a datagram the
/// same size as a real one -- any content that fails the USB sync-byte
/// check at that length is equally "malformed" to the framing validator.
const FAKE_METIS_PACKET_LEN: usize = 1032;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills and reaps the daemon child process on drop, including on a
/// failing assertion (Rust's default unwind panic strategy still runs
/// `Drop` impls during unwinding) -- so a failing assert never leaks a
/// `manta` process into the CI runner.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Stops and joins the fake-device thread on drop, same rationale as
/// `ChildGuard`.
struct DeviceGuard {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The value trailing a Prometheus sample line whose text starts with
/// `metric_with_labels` (e.g. `manta_input_malformed_packets_total{source="hpsdr"}`).
fn metric_value(body: &str, metric_with_labels: &str) -> Option<u64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find(|l| l.starts_with(prefix.as_str()))
        .and_then(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

fn scrape_metrics(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .ok()?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    Some(body)
}

#[test]
fn malformed_hpsdr_datagrams_are_visible_on_the_metrics_endpoint() {
    // 1. Fake device: a loopback UDP socket that discards whatever the
    //    daemon sends it (the initial C&C/start-command burst, ongoing
    //    keepalives) and drips one 1032-byte garbage datagram back every
    //    100 ms until stopped. Never sends a valid Metis packet, so
    //    `manta_source_health{source="hpsdr"}` must stay 0 for the life of
    //    the test -- asserted below as a bonus MAN-55 regression guard.
    let device_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    device_socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let device_port = device_socket.local_addr().unwrap().port();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = thread::spawn(move || {
        let mut client_addr = None;
        // First drip fires as soon as a client address is known -- see the
        // unconditional `>= Duration::from_millis(100)` check below, which
        // an elapsed() of ~0 immediately satisfies on the very first loop
        // iteration once `client_addr` is `Some`.
        let mut last_drip = Instant::now() - Duration::from_millis(100);
        let mut buf = [0u8; 65536];
        while !stop_thread.load(Ordering::Relaxed) {
            if let Ok((_, addr)) = device_socket.recv_from(&mut buf) {
                client_addr = Some(addr);
            }
            if let Some(addr) = client_addr {
                if last_drip.elapsed() >= Duration::from_millis(100) {
                    let garbage = [0xFFu8; FAKE_METIS_PACKET_LEN];
                    let _ = device_socket.send_to(&garbage, addr);
                    last_drip = Instant::now();
                }
            }
        }
    });
    let _device_guard = DeviceGuard {
        stop,
        handle: Some(handle),
    };

    // 2. Config: three free ports picked by TcpListener::bind + drop (no
    //    fixed ports -- CLAUDE.md multi-agent hygiene).
    let telnet_port = free_port();
    let json_port = free_port();
    let metrics_port = free_port();
    let mut cfg_file = tempfile::NamedTempFile::new().unwrap();
    write!(
        cfg_file,
        r#"
        [server]
        station_callsign = "W1AW"
        bind_addr = "127.0.0.1"
        telnet_port = {telnet_port}
        json_port = {json_port}
        metrics_port = {metrics_port}
        "#
    )
    .unwrap();
    cfg_file.flush().unwrap();

    // 3. Spawn the daemon as a real child process against the fake device.
    let child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("listen")
        .arg("--hpsdr-host")
        .arg("127.0.0.1")
        .arg("--hpsdr-port")
        .arg(device_port.to_string())
        .arg("--hpsdr-freq")
        .arg("14025000")
        .arg("--hpsdr-rate")
        .arg("192000")
        .arg("--server-config")
        .arg(cfg_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn manta listen child process");
    let _child_guard = ChildGuard(child);

    // 4. Poll /metrics for up to 20 s, succeeding as soon as the
    //    malformed-packet counter is observed > 0. Polling a monotonic
    //    counter to a threshold (rather than asserting an exact value
    //    once) is what keeps this non-flaky under CI scheduling jitter --
    //    the test should finish in ~1-2s; the 20s budget is headroom, not
    //    the expected runtime.
    let malformed_metric = r#"manta_input_malformed_packets_total{source="hpsdr"}"#;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_body = String::new();
    loop {
        if let Some(body) = scrape_metrics(metrics_port) {
            let malformed = metric_value(&body, malformed_metric);
            last_body = body;
            if malformed.unwrap_or(0) > 0 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "malformed packets never became visible on /metrics within 20s; last scrape:\n{last_body}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // 5. Assert on the winning body.
    assert!(
        metric_value(&last_body, malformed_metric).unwrap_or(0) > 0,
        "body: {last_body}"
    );
    assert_eq!(
        metric_value(
            &last_body,
            r#"manta_input_dropped_packets_total{source="hpsdr"}"#
        ),
        Some(0),
        "the series must exist (eager publish, MAN-56 D6) even at zero -- \
         no valid Metis packet ever arrived, so nothing should be flagged \
         dropped/gapped: {last_body}"
    );
    assert_eq!(
        metric_value(
            &last_body,
            r#"manta_input_gaps_detected_total{source="hpsdr"}"#
        ),
        Some(0),
        "body: {last_body}"
    );
    assert_eq!(
        metric_value(&last_body, r#"manta_source_health{source="hpsdr"}"#),
        Some(0),
        "no valid Metis packet was ever sent -- MAN-55's semantics must still hold: {last_body}"
    );
}
