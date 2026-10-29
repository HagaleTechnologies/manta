//! MAN-13 acceptance, scenarios 2 and 3: several receiver channels of one
//! SDR (ka9q-radio SSRCs on one radiod) feed one `manta run`, and one
//! channel failing leaves the others spotting.
//!
//! A fake radiod in this process sends radiod's wire format over loopback
//! multicast with TTL 0, as `ka9q_input.rs` does, once per channel: each
//! channel has its own SSRC and status, and every channel shares one data
//! group and port. Gated to Linux and macOS like `ka9q_input.rs` (D19 in
//! docs/DECISIONS/2026-10-13-man140-ka9q-radio-input.md).
#![cfg(any(target_os = "linux", target_os = "macos"))]

use manta_input::ka9q::wire::{self, ChannelStatus, Encoding, RtpHeader};
use num_complex::Complex32;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RATE: u64 = 96_000;
const PER_PACKET: usize = 180;
const READY_MARKER: &str = "manta: listening on";

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

static NEXT: AtomicU8 = AtomicU8::new(101);

/// A group unique to this test process and call (`ka9q_input.rs` counts
/// from 1, so the two files never share one).
fn unique_group() -> Ipv4Addr {
    let pid = std::process::id();
    Ipv4Addr::new(
        239,
        (pid % 250) as u8 + 1,
        (pid / 250 % 250) as u8,
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

/// `N` distinct ports, every listener held until all are read (MAN-371).
fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Loopback interface, TTL 0, loop on.
fn sender() -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
    s.set_multicast_ttl_v4(0).unwrap();
    s.set_multicast_loop_v4(true).unwrap();
    s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
        .unwrap();
    s.into()
}

/// `seconds` of the testkit's AWGN with no signal, then `call` keying V1's
/// text at +12.34 kHz for 30 s, at 96 kS/s.
fn channel_samples(call: Option<&str>, noise_s: f64, seed: u64) -> Vec<Complex32> {
    let mut out = Vec::new();
    if noise_s > 0.0 {
        let (noise, _) =
            manta_testkit::scene::render_scene(&[], RATE as f64, noise_s, Some(seed)).unwrap();
        out.extend(noise);
    }
    if let Some(call) = call {
        let mut spec = manta_testkit::vectors::VectorSpec {
            duration_s: 30.0,
            noise_seed: seed ^ 0x5a5a,
            ..manta_testkit::vectors::v1()
        };
        spec.signals[0].text = format!("CQ CQ DE {call} {call} K");
        out.extend(manta_testkit::vectors::render(&spec).unwrap().samples);
    }
    out
}

fn channel_status(group: Ipv4Addr, data_port: u16, ssrc: u32, freq_hz: f64) -> ChannelStatus {
    ChannelStatus {
        ssrc: Some(ssrc),
        sample_rate_hz: Some(RATE),
        radio_frequency_hz: Some(freq_hz),
        low_edge_hz: Some(-45_000.0),
        high_edge_hz: Some(45_000.0),
        channels: Some(2),
        isb: Some(false),
        envelope: Some(false),
        agc: Some(false),
        rtp_pt: Some(111),
        encoding: Some(Encoding::F32Le.wire_value()),
        data_dest: Some(SocketAddr::V4(SocketAddrV4::new(group, data_port))),
    }
}

/// One fake radiod channel: its status to `group:5006` every 500 ms until
/// stopped, and its samples as RTP once `stream` is called.
struct FakeChannel {
    ssrc: u32,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl FakeChannel {
    fn start(group: Ipv4Addr, data_port: u16, ssrc: u32, freq_hz: f64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let sp = stop.clone();
        let status = channel_status(group, data_port, ssrc, freq_hz);
        let t = thread::spawn(move || {
            let tx = sender();
            let pkt = wire::encode_status_packet(&status);
            while !sp.load(Ordering::Relaxed) {
                let _ = tx.send_to(&pkt, SocketAddrV4::new(group, wire::STATUS_PORT));
                for _ in 0..10 {
                    if sp.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        });
        Self {
            ssrc,
            stop,
            threads: vec![t],
        }
    }

    /// Sends `samples` once as RTP to `group:port` at real time. With
    /// `then_silent`, the channel's status stops too when they end: the
    /// channel disappears from radiod.
    fn stream(&mut self, group: Ipv4Addr, port: u16, samples: Vec<Complex32>, then_silent: bool) {
        let sp = self.stop.clone();
        let ssrc = self.ssrc;
        let t = thread::spawn(move || {
            let tx = sender();
            let dest = SocketAddrV4::new(group, port);
            let packet_s = PER_PACKET as f64 / RATE as f64;
            let start = Instant::now();
            let mut ts: u32 = 0;
            for (i, chunk) in samples.chunks(PER_PACKET).enumerate() {
                if sp.load(Ordering::Relaxed) {
                    return;
                }
                let due = start + Duration::from_secs_f64(i as f64 * packet_s);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
                let h = RtpHeader {
                    payload_type: 111,
                    seq: i as u16,
                    timestamp: ts,
                    ssrc,
                    marker: false,
                };
                ts = ts.wrapping_add(chunk.len() as u32);
                let pkt = wire::encode_rtp(&h, &wire::encode_iq(Encoding::F32Le, chunk));
                let _ = tx.send_to(&pkt, dest);
            }
            if then_silent {
                sp.store(true, Ordering::Relaxed);
            }
        });
        self.threads.push(t);
    }
}

impl Drop for FakeChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_string());
    body
}

fn metric_value(body: &str, metric_with_labels: &str) -> Option<u64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find(|l| l.starts_with(prefix.as_str()))
        .and_then(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

/// One `[[input]]` per `(name, ssrc)`, all on `group:port`, plus a
/// `[server]` on loopback with a 1 s status line.
fn write_config(dir: &Path, group: Ipv4Addr, port: u16, channels: &[(&str, u32)]) -> (u16, u16) {
    let [telnet_port, json_port, metrics_port] = free_ports();
    let mut cfg = format!(
        "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\nstatus_interval_secs = 1\n"
    );
    for (name, ssrc) in channels {
        cfg.push_str(&format!(
            "\n[[input]]\nname = \"{name}\"\ntype = \"ka9q\"\ngroup = \"{group}\"\nport = {port}\nssrc = {ssrc}\n"
        ));
    }
    std::fs::write(dir.join("manta.toml"), cfg).unwrap();
    (json_port, metrics_port)
}

/// A running `manta run --json --config manta.toml`: its spots, and every
/// stderr line with the time it arrived.
struct Daemon {
    _guard: ChildGuard,
    spots: mpsc::Receiver<(Instant, serde_json::Value)>,
    stderr: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl Daemon {
    fn start(dir: &Path) -> Self {
        let mut child = manta()
            .args(["run", "--json", "--config"])
            .arg(dir.join("manta.toml"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        {
            let stderr = stderr.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                    if line.contains(READY_MARKER) {
                        let _ = ready_tx.send(());
                    }
                    stderr.lock().unwrap().push((Instant::now(), line));
                }
            });
        }
        let (spot_tx, spots) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(s) = v.get("spot") {
                        let _ = spot_tx.send((Instant::now(), s.clone()));
                    }
                }
            }
        });
        let daemon = Self {
            _guard: ChildGuard(child),
            spots,
            stderr,
        };
        if ready_rx.recv_timeout(Duration::from_secs(60)).is_err() {
            panic!(
                "manta never printed {READY_MARKER:?}; stderr:\n{}",
                daemon.stderr_text()
            );
        }
        daemon
    }

    fn stderr_text(&self) -> String {
        let lines = self.stderr.lock().unwrap();
        lines
            .iter()
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// When the first stderr line containing `needle` arrived.
    fn line_at(&self, needle: &str) -> Option<Instant> {
        let lines = self.stderr.lock().unwrap();
        lines
            .iter()
            .find(|(_, l)| l.contains(needle))
            .map(|(at, _)| *at)
    }

    /// Spots until every call in `want` has one, or `timeout`.
    fn spots_until(&self, want: &[&str], timeout: Duration) -> Vec<(Instant, serde_json::Value)> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let calls: BTreeSet<&str> = seen
                .iter()
                .filter_map(|(_, s): &(Instant, serde_json::Value)| s["callsign"].as_str())
                .collect();
            if want.iter().all(|c| calls.contains(c)) {
                return seen;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.spots.recv_timeout(left) {
                Ok(spot) => seen.push(spot),
                Err(_) => panic!(
                    "spots {:?} never all arrived; got {:?}; stderr:\n{}",
                    want,
                    seen.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>(),
                    self.stderr_text()
                ),
            }
        }
    }
}

/// Gherkin (MAN-13, scenario 2):
///   Given one SDR is configured with several independent receiver
///   channels, each its own segment
///   When manta runs as a single daemon configured with all of those
///   channels
///   Then decoded spots from all channels are emitted from the same shared
///   spot output
#[test]
fn two_ka9q_channels_on_one_radiod_feed_one_spot_stream() {
    let dir = tempfile::tempdir().unwrap();
    let group = unique_group();
    let data_port = free_udp_port();
    let mut twenty = FakeChannel::start(group, data_port, 14_000_000, 14_000_000.0);
    let mut forty = FakeChannel::start(group, data_port, 7_000_000, 7_000_000.0);
    let (_, metrics_port) = write_config(
        dir.path(),
        group,
        data_port,
        &[("20m", 14_000_000), ("40m", 7_000_000)],
    );
    let daemon = Daemon::start(dir.path());

    twenty.stream(
        group,
        data_port,
        channel_samples(Some("W1AW"), 0.0, 11),
        false,
    );
    forty.stream(
        group,
        data_port,
        channel_samples(Some("JA1ABC"), 0.0, 22),
        false,
    );

    let spots = daemon.spots_until(&["W1AW", "JA1ABC"], Duration::from_secs(90));
    for (_, spot) in &spots {
        let call = spot["callsign"].as_str().unwrap_or_default();
        let freq = spot["freq_hz"].as_f64().unwrap_or_default();
        match call {
            "W1AW" => assert!((freq - 14_012_340.0).abs() < 100.0, "{spot}"),
            "JA1ABC" => assert!((freq - 7_012_340.0).abs() < 100.0, "{spot}"),
            other => panic!("unexpected spot {other}: {spot}"),
        }
    }
    let body = http_get(metrics_port, "/metrics").unwrap_or_default();
    assert_eq!(
        metric_value(&body, r#"manta_source_health{source="20m"}"#),
        Some(1),
        "{body}"
    );
    assert_eq!(
        metric_value(&body, r#"manta_source_health{source="40m"}"#),
        Some(1),
        "{body}"
    );
}

/// Gherkin (MAN-13, scenario 3):
///   Given a daemon is running with three configured IQ sources
///   When one source disconnects or fails
///   Then the daemon keeps decoding and spotting from the remaining sources
///   And the failed source's state is visible in metrics/logs
///
/// Channel c delivers 3 s of samples and then disappears from radiod
/// (status and RTP stop), so ka9q's 10 s stall bound fails it about 13 s in.
/// Channels a and b carry noise until 25 s and only then a station, so
/// their spots can only arrive after c has failed.
#[test]
fn one_of_three_ka9q_inputs_stopping_leaves_the_others_spotting() {
    let dir = tempfile::tempdir().unwrap();
    let group = unique_group();
    let data_port = free_udp_port();
    let mut a = FakeChannel::start(group, data_port, 14_000_000, 14_000_000.0);
    let mut b = FakeChannel::start(group, data_port, 7_000_000, 7_000_000.0);
    let mut c = FakeChannel::start(group, data_port, 21_000_000, 21_000_000.0);
    let (json_port, metrics_port) = write_config(
        dir.path(),
        group,
        data_port,
        &[("a", 14_000_000), ("b", 7_000_000), ("c", 21_000_000)],
    );
    let _ = json_port;
    let daemon = Daemon::start(dir.path());

    a.stream(
        group,
        data_port,
        channel_samples(Some("W1AW"), 25.0, 11),
        false,
    );
    b.stream(
        group,
        data_port,
        channel_samples(Some("JA1ABC"), 25.0, 22),
        false,
    );
    c.stream(group, data_port, channel_samples(None, 3.0, 33), true);

    let spots = daemon.spots_until(&["W1AW", "JA1ABC"], Duration::from_secs(120));
    let lost = daemon
        .line_at("input c: lost")
        .unwrap_or_else(|| panic!("no `input c: lost` line; stderr:\n{}", daemon.stderr_text()));
    for (at, spot) in &spots {
        assert!(
            *at > lost,
            "spot {spot} arrived before input c was lost; stderr:\n{}",
            daemon.stderr_text()
        );
    }
    let err = daemon.stderr_text();
    assert!(err.contains("2 of 3 inputs decoding"), "{err}");

    let body = http_get(metrics_port, "/metrics").unwrap_or_default();
    for (source, want) in [("a", 1), ("b", 1), ("c", 0)] {
        assert_eq!(
            metric_value(
                &body,
                &format!(r#"manta_source_health{{source="{source}"}}"#)
            ),
            Some(want),
            "{body}"
        );
    }
    assert!(
        metric_value(&body, r#"manta_source_outages_total{source="c"}"#).unwrap_or(0) >= 1,
        "{body}"
    );
    assert_eq!(metric_value(&body, "manta_healthy"), Some(1), "{body}");
    let health = http_get(metrics_port, "/healthz").unwrap_or_default();
    assert_eq!(health.lines().next(), Some("ok"), "{health}");
    assert!(
        health.lines().any(|l| l == "source c: unhealthy"),
        "{health}"
    );

    // The status line reports the healthy inputs out of the configured.
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.line_at(" inputs=2/3").is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
    }
    assert!(
        daemon.line_at(" inputs=2/3").is_some(),
        "no status line with inputs=2/3; stderr:\n{}",
        daemon.stderr_text()
    );
}
