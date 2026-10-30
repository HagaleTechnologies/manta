//! MAN-140 acceptance: manta decodes and spots from a ka9q-radio (radiod)
//! multicast IQ channel the same way it does from any other `IqSource`.
//!
//! A fake radiod in this process sends radiod's wire format (status TLV
//! packets to `group:5006`, RTP IQ to `group:port`) over loopback multicast
//! with TTL 0, and a real `manta` child joins the group. Gated to Linux and
//! macOS (D19 in docs/DECISIONS/2026-10-13-man140-ka9q-radio-input.md):
//! loopback multicast is unverified on Windows CI.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use manta_input::ka9q::wire::{self, ChannelStatus, Encoding, RtpHeader};
use manta_input::IqSource;
use num_complex::Complex32;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SSRC: u32 = 14_000_000;
const RATE: u64 = 96_000;
const PER_PACKET: usize = 180;
const READY_MARKER: &str = "manta: listening;";

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

static NEXT: AtomicU8 = AtomicU8::new(1);

/// A group unique to this test process and call.
fn unique_group() -> Ipv4Addr {
    let pid = std::process::id();
    Ipv4Addr::new(
        239,
        (pid % 250) as u8 + 1,
        (pid / 250 % 250) as u8,
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

/// `N` distinct ports nothing listens on right now. All `N` listeners are
/// held until every port is read: releasing each before binding the next
/// lets the OS hand the same port out twice (Windows does, and then the
/// config is rejected as "two servers cannot share a port").
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

/// The prototype's sender: loopback interface, TTL 0, loop on.
fn sender() -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
    s.set_multicast_ttl_v4(0).unwrap();
    s.set_multicast_loop_v4(true).unwrap();
    s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
        .unwrap();
    s.into()
}

/// V1 shortened to 30 s, as `tests/cli.rs`'s `short_v1`.
fn v1_fixture(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

fn wav_samples(path: &Path) -> Vec<Complex32> {
    let mut src = manta_input::WavIqSource::open(path).unwrap();
    assert_eq!(src.sample_rate(), RATE as f64);
    let mut out = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); 65536];
    loop {
        let n = src.read(&mut buf).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn channel_status(group: Ipv4Addr, data_port: u16, agc: bool) -> ChannelStatus {
    ChannelStatus {
        ssrc: Some(SSRC),
        sample_rate_hz: Some(RATE),
        radio_frequency_hz: Some(14_000_000.0),
        low_edge_hz: Some(-45_000.0),
        high_edge_hz: Some(45_000.0),
        channels: Some(2),
        isb: Some(false),
        envelope: Some(false),
        agc: Some(agc),
        rtp_pt: Some(111),
        encoding: Some(Encoding::F32Le.wire_value()),
        data_dest: Some(SocketAddr::V4(SocketAddrV4::new(group, data_port))),
    }
}

/// A fake radiod: status to `group:5006` every 500 ms, and RTP when told.
struct FakeRadiod {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl FakeRadiod {
    fn status_only(group: Ipv4Addr, status: ChannelStatus) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let sp = stop.clone();
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
            stop,
            threads: vec![t],
        }
    }

    /// Sends `samples` as RTP to `group:port`, `speed` times real time,
    /// skipping packet index `skip`; loops when `looping`. Signals `done`
    /// after the first pass.
    #[allow(clippy::too_many_arguments)]
    fn stream(
        &mut self,
        group: Ipv4Addr,
        port: u16,
        samples: Arc<Vec<Complex32>>,
        speed: f64,
        skip: Option<usize>,
        looping: bool,
        done: mpsc::Sender<()>,
    ) {
        let sp = self.stop.clone();
        let t = thread::spawn(move || {
            let tx = sender();
            let dest = SocketAddrV4::new(group, port);
            let packet_s = PER_PACKET as f64 / RATE as f64 / speed;
            let mut seq: u16 = 0;
            let mut ts: u32 = 0;
            loop {
                let start = Instant::now();
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
                        seq,
                        timestamp: ts,
                        ssrc: SSRC,
                        marker: false,
                    };
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(chunk.len() as u32);
                    if skip == Some(i) {
                        continue;
                    }
                    let pkt = wire::encode_rtp(&h, &wire::encode_iq(Encoding::F32Le, chunk));
                    let _ = tx.send_to(&pkt, dest);
                }
                let _ = done.send(());
                if !looping {
                    return;
                }
            }
        });
        self.threads.push(t);
    }
}

impl Drop for FakeRadiod {
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
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    Some(body)
}

/// The single spot of `manta run --source <wav> --source-iq --json`.
fn file_replay_spot(wav: &Path) -> (String, f64) {
    let out = manta()
        .args(["run", "--source"])
        .arg(wav)
        .args(["--source-iq", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let spots: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("spot").cloned())
        .collect();
    assert_eq!(spots.len(), 1, "file replay spots: {spots:?}");
    (
        spots[0]["dxCall"].as_str().unwrap().to_string(),
        spots[0]["frequency"].as_f64().unwrap(),
    )
}

/// Gherkin (MAN-140):
///   Given ka9q-radio is streaming IQ over an RTP multicast group
///   When manta is configured to join that group as its input source
///   Then it decodes and spots from that stream the same way it does from
///   any other IqSource
#[test]
fn ka9q_stream_spots_w1aw_like_the_file_replay() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let (file_call, file_freq) = file_replay_spot(&wav);
    let samples = Arc::new(wav_samples(&wav));

    let group = unique_group();
    let data_port = free_udp_port();
    let mut fake = FakeRadiod::status_only(group, channel_status(group, data_port, false));

    let [telnet_port, json_port, metrics_port] = free_ports();
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\n"
        ),
    )
    .unwrap();
    let mut child = manta()
        .args(["run", "--ka9q-group"])
        .arg(group.to_string())
        .args(["--ka9q-port", &data_port.to_string(), "--json", "--config"])
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _guard = ChildGuard(child);

    // stderr: signal the ready line, keep every line for the final check.
    let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ready_tx, ready_rx) = mpsc::channel();
    {
        let err_lines = err_lines.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains(READY_MARKER) {
                    let _ = ready_tx.send(());
                }
                err_lines.lock().unwrap().push(line);
            }
        });
    }
    let (spot_tx, spot_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(s) = v.get("spot") {
                    let _ = spot_tx.send(s.clone());
                }
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "manta never printed {READY_MARKER:?}; stderr:\n{}",
                err_lines.lock().unwrap().join("\n")
            )
        });

    // The source has joined and accepted the status: start RTP from sample
    // 0 at 2x real time, dropping the packet at 25.0 s (after the spot).
    let skip = (25.0 * RATE as f64 / PER_PACKET as f64) as usize;
    let (done_tx, done_rx) = mpsc::channel();
    fake.stream(group, data_port, samples, 2.0, Some(skip), false, done_tx);

    let spot = spot_rx
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| {
            panic!(
                "no spot within 60 s; stderr:\n{}",
                err_lines.lock().unwrap().join("\n")
            )
        });
    assert_eq!(spot["dxCall"].as_str(), Some(file_call.as_str()));
    assert_eq!(file_call, "W1AW");
    let freq = spot["frequency"].as_f64().unwrap();
    assert!(
        (freq - file_freq).abs() <= 1.0,
        "ka9q spot at {freq} Hz, file replay at {file_freq} Hz"
    );

    done_rx.recv_timeout(Duration::from_secs(60)).unwrap();
    // Let the last packets and a metrics tick land.
    let deadline = Instant::now() + Duration::from_secs(10);
    let body = loop {
        let body = scrape_metrics(metrics_port).unwrap_or_default();
        let ok = metric_value(&body, r#"manta_source_health{source="ka9q"}"#) == Some(1)
            && metric_value(&body, r#"manta_input_dropped_packets_total{source="ka9q"}"#)
                .unwrap_or(0)
                >= 1
            && metric_value(&body, r#"manta_input_gaps_detected_total{source="ka9q"}"#)
                .unwrap_or(0)
                >= 1;
        if ok || Instant::now() >= deadline {
            break body;
        }
        thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(
        metric_value(&body, r#"manta_source_health{source="ka9q"}"#),
        Some(1),
        "{body}"
    );
    assert!(
        metric_value(&body, r#"manta_input_dropped_packets_total{source="ka9q"}"#).unwrap_or(0)
            >= 1,
        "{body}"
    );
    assert!(
        metric_value(&body, r#"manta_input_gaps_detected_total{source="ka9q"}"#).unwrap_or(0) >= 1,
        "{body}"
    );
    let err = err_lines.lock().unwrap().join("\n");
    assert!(!err.contains("source ka9q lost:"), "{err}");
}

/// Runs `manta check --ka9q-group <group> --ka9q-port <port> --duration 1`
/// against a fake looping V1's first 3 s at 1x.
fn check_against_fake(agc: bool) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let mut samples = wav_samples(&wav);
    samples.truncate(3 * RATE as usize);
    let group = unique_group();
    let data_port = free_udp_port();
    let mut fake = FakeRadiod::status_only(group, channel_status(group, data_port, agc));
    let (done_tx, _done_rx) = mpsc::channel();
    fake.stream(
        group,
        data_port,
        Arc::new(samples),
        1.0,
        None,
        true,
        done_tx,
    );
    let out = manta()
        .args(["check", "--ka9q-group"])
        .arg(group.to_string())
        .args(["--ka9q-port", &data_port.to_string(), "--duration", "1"])
        .output()
        .unwrap();
    drop(fake);
    out
}

#[test]
fn ka9q_check_reports_the_channel_rate_frequency_and_passband() {
    let out = check_against_fake(false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    for want in [
        "Source: ka9q",
        "Stream sample rate: 96000 Hz",
        "Center frequency: 14000000 Hz",
        "Passband offsets: -45000 to 45000 Hz",
    ] {
        assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}");
    }
    assert!(!stderr.contains("has AGC on"), "{stderr}");
}

#[test]
fn ka9q_without_radiod_fails_naming_ttl() {
    let t = Instant::now();
    let out = manta()
        .args(["check", "--ka9q-group"])
        .arg(unique_group().to_string())
        .args(["--ka9q-port", &free_udp_port().to_string()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(t.elapsed() < Duration::from_secs(20));
    assert!(stderr.contains("no ka9q-radio status from"), "{stderr}");
    assert!(stderr.contains("ttl"), "{stderr}");
}

#[test]
fn ka9q_warns_when_the_channel_has_agc_on() {
    let out = check_against_fake(true);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("has AGC on; set agc = off"), "{stderr}");
}

/// D13: a radiod retune ends `run` with exit 1, with or without
/// `--dial-freq-hz`. The override must not hide the retune from
/// `ReconnectingSource`'s reopen check, or spots would be published against
/// the old frequency.
#[test]
fn ka9q_retune_ends_run_even_with_a_dial_override() {
    for dial in [None, Some("14000000")] {
        let group = unique_group();
        let data_port = free_udp_port();
        let mut rtp = FakeRadiod {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let (done_tx, _done_rx) = mpsc::channel();
        rtp.stream(
            group,
            data_port,
            Arc::new(vec![Complex32::new(0.01, 0.0); RATE as usize]),
            1.0,
            None,
            true,
            done_tx,
        );
        let status_a = FakeRadiod::status_only(group, channel_status(group, data_port, false));

        let mut cmd = manta();
        cmd.args(["run", "--ka9q-group"])
            .arg(group.to_string())
            .args(["--ka9q-port", &data_port.to_string()])
            .args(["--ka9q-ssrc", &SSRC.to_string()]);
        if let Some(hz) = dial {
            cmd.args(["--dial-freq-hz", hz]);
        }
        let mut child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut guard = ChildGuard(child);
        let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let reader = {
            let err_lines = err_lines.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.contains(READY_MARKER) {
                        let _ = ready_tx.send(());
                    }
                    err_lines.lock().unwrap().push(line);
                }
            })
        };
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                panic!(
                    "dial {dial:?}: manta never printed {READY_MARKER:?}; stderr:\n{}",
                    err_lines.lock().unwrap().join("\n")
                )
            });

        // Retune the channel in "radiod": 14000.0 kHz to 14050.0 kHz.
        drop(status_a);
        let _status_b = FakeRadiod::status_only(
            group,
            ChannelStatus {
                radio_frequency_hz: Some(14_050_000.0),
                ..channel_status(group, data_port, false)
            },
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        let code = loop {
            if let Some(status) = guard.0.try_wait().unwrap() {
                break status.code();
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(100));
        };
        if code.is_some() {
            let _ = reader.join();
        }
        let err = err_lines.lock().unwrap().join("\n");
        assert_eq!(code, Some(1), "dial {dial:?}: stderr:\n{err}");
        assert!(
            err.contains("retuned from 14000.0 kHz to 14050.0 kHz"),
            "dial {dial:?}: {err}"
        );
        assert!(
            err.contains("source ka9q reopened with a different center frequency"),
            "dial {dial:?}: {err}"
        );
    }
}

/// MAN-334: a ka9q-radio channel has no configured centre or passband, so
/// calibrate plans on what radiod reports once joined -- the re-plan after
/// open that every source gets.
#[test]
fn ka9q_calibrate_plans_on_the_frequency_radiod_reports() {
    let group = unique_group();
    let data_port = free_udp_port();
    let status = ChannelStatus {
        radio_frequency_hz: Some(7_030_000.0),
        ..channel_status(group, data_port, false)
    };
    let fake = FakeRadiod::status_only(group, status);
    let out = manta()
        .args(["calibrate", "--ka9q-group"])
        .arg(group.to_string())
        .args(["--ka9q-port", &data_port.to_string(), "--duration", "10"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    drop(fake);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(
            "no known reference frequency is usable in this receiver's passband \
             (6985000.0 to 7075000.0 Hz)"
        ),
        "{stderr}"
    );
}
