//! MAN-307 acceptance against real child processes and a loopback Protocol 1 radio.
use num_complex::Complex32;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type IqSamples = Arc<Vec<Vec<Complex32>>>;

const RATE: f64 = 192000.0;
const NAMES: [&str; 4] = ["80m", "40m", "20m", "15m"];
const FREQUENCIES: [u32; 4] = [3525000, 7025000, 14025000, 21025000];
const CALLS: [&str; 4] = ["W1AW", "JA1ABC", "DL1ABC", "VK2ABC"];

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}
fn ports() -> [u16; 3] {
    let listeners: [TcpListener; 3] =
        std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    listeners.map(|l| l.local_addr().unwrap().port())
}
fn config(
    path: &Path,
    port: u16,
    count: usize,
    declaration: Option<usize>,
    transforms: bool,
) -> [u16; 3] {
    let ports = ports();
    let mut text = format!("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {}\njson_port = {}\nmetrics_port = {}\nstatus_interval_secs = 1\n", ports[0], ports[1], ports[2]);
    for i in 0..count {
        let name = NAMES
            .get(i)
            .map_or_else(|| format!("extra{i}"), |s| (*s).to_string());
        let freq = FREQUENCIES[i % 4];
        text += &format!("\n[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\nport = {port}\nfreq_hz = {freq}.0\nrate_hz = {RATE}.0\n");
        if i == 0 {
            if let Some(n) = declaration {
                text += &format!("receivers = {n}\n");
            }
        }
        if transforms && i == 1 {
            text += "capture_rate_hz = 48000.0\ncenter_freq_hz = 7100000.0\nfreq_correction_ppm = 100.0\n";
        }
    }
    std::fs::write(path, text).unwrap();
    ports
}

#[derive(Default, Clone, Debug)]
struct Session {
    starts: usize,
    stops: usize,
    count: Option<u8>,
    frequencies: BTreeMap<u8, u32>,
}
struct Radio {
    port: u16,
    samples: Arc<Mutex<Option<IqSamples>>>,
    mode: Arc<AtomicU8>, // 0 malformed, 1 IQ, 2 silent
    stop: Arc<AtomicBool>,
    sessions: Arc<Mutex<BTreeMap<SocketAddr, Session>>>,
    queries: Arc<Mutex<usize>>,
    worker: Option<JoinHandle<()>>,
}
fn fixture() -> IqSamples {
    static SAMPLES: OnceLock<IqSamples> = OnceLock::new();
    SAMPLES
        .get_or_init(|| {
            Arc::new(
                CALLS
                    .iter()
                    .enumerate()
                    .map(|(i, call)| {
                        let mut spec = manta_testkit::vectors::VectorSpec {
                            fs: RATE,
                            duration_s: 24.0,
                            noise_seed: 701 + i as u64,
                            ..manta_testkit::vectors::v1()
                        };
                        spec.signals[0].text = format!("CQ CQ DE {call} {call} K");
                        manta_testkit::vectors::render(&spec).unwrap().samples
                    })
                    .collect(),
            )
        })
        .clone()
}
fn iq_packet(receivers: usize, seq: u32, start: usize, samples: &[Vec<Complex32>]) -> [u8; 1032] {
    let mut packet = [0; 1032];
    packet[..4].copy_from_slice(&[0xef, 0xfe, 1, 6]);
    packet[4..8].copy_from_slice(&seq.to_be_bytes());
    let per_frame = 504 / (6 * receivers + 2);
    for frame in 0..2 {
        let base = 8 + frame * 512;
        packet[base..base + 3].fill(0x7f);
        for n in 0..per_frame {
            for rx in 0..receivers {
                let sample = samples[rx % samples.len()]
                    [(start + frame * per_frame + n) % samples[rx % samples.len()].len()];
                let offset = base + 8 + n * (6 * receivers + 2) + rx * 6;
                for (component, value) in [sample.re, sample.im].into_iter().enumerate() {
                    let signed = (value.clamp(-0.99999, 0.99999) * 8388608.0).round() as i32;
                    packet[offset + component * 3..offset + component * 3 + 3]
                        .copy_from_slice(&signed.to_be_bytes()[1..]);
                }
            }
        }
    }
    packet
}
impl Radio {
    fn start(capacity: Option<u8>, samples: Option<IqSamples>) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let port = socket.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU8::new(0));
        let sessions = Arc::new(Mutex::new(BTreeMap::<SocketAddr, Session>::new()));
        let queries = Arc::new(Mutex::new(0));
        let (sp, mode_thread, records, query_count) = (
            stop.clone(),
            mode.clone(),
            sessions.clone(),
            queries.clone(),
        );
        let samples = Arc::new(Mutex::new(samples));
        let current_samples = samples.clone();
        let worker = thread::spawn(move || {
            let mut buf = [0; 65536];
            let mut current = None;
            let mut cursor = 0;
            let mut seq = 0u32;
            let mut due = Instant::now();
            while !sp.load(Ordering::Relaxed) {
                while let Ok((n, peer)) = socket.recv_from(&mut buf) {
                    if n == 60 && buf[..3] == [0xef, 0xfe, 2] {
                        *query_count.lock().unwrap() += 1;
                        let mut reply = [0; 60];
                        reply[..3].copy_from_slice(&[0xef, 0xfe, 2]);
                        reply[10] = if capacity.is_some() { 6 } else { 1 };
                        reply[19] = capacity.unwrap_or(0);
                        let _ = socket.send_to(&reply, peer);
                    } else if n == 1032 && buf[..4] == [0xef, 0xfe, 1, 2] {
                        let mut records = records.lock().unwrap();
                        let record = records.entry(peer).or_default();
                        for offset in [8, 520] {
                            assert_eq!(&buf[offset..offset + 3], &[0x7f; 3]);
                            let address = buf[offset + 3] >> 1;
                            if address == 0 {
                                record.count = Some(((buf[offset + 7] >> 3) & 7) + 1);
                            }
                            if (2..=8).contains(&address) {
                                record.frequencies.insert(
                                    address - 2,
                                    u32::from_be_bytes(
                                        buf[offset + 4..offset + 8].try_into().unwrap(),
                                    ),
                                );
                            }
                        }
                    } else if n == 60 && buf[..3] == [0xef, 0xfe, 4] {
                        if buf[3] == 1 {
                            records.lock().unwrap().entry(peer).or_default().starts += 1;
                            if current != Some(peer) {
                                cursor = 0;
                                seq = 0;
                                due = Instant::now();
                            }
                            current = Some(peer);
                        } else {
                            records.lock().unwrap().entry(peer).or_default().stops += 1;
                            if current == Some(peer) {
                                current = None;
                            }
                        }
                    }
                }
                if let Some(peer) = current {
                    let mode = mode_thread.load(Ordering::Relaxed);
                    if mode == 0 && Instant::now() >= due {
                        let _ = socket.send_to(&[0xff; 1032], peer);
                        due = Instant::now() + Duration::from_millis(20);
                    } else if mode == 1 {
                        let samples = current_samples.lock().unwrap().clone();
                        if let Some(samples) = &samples {
                            let receivers = records.lock().unwrap()[&peer].count.unwrap() as usize;
                            let per_packet = 2 * (504 / (6 * receivers + 2));
                            // Bound each burst so control/stop traffic is serviced
                            // even when the test runner falls behind wall time.
                            for _ in 0..64 {
                                if Instant::now() < due {
                                    break;
                                }
                                let packet = iq_packet(receivers, seq, cursor, samples);
                                let _ = socket.send_to(&packet, peer);
                                seq = seq.wrapping_add(1);
                                cursor += per_packet;
                                due += Duration::from_secs_f64(per_packet as f64 / RATE);
                            }
                        }
                    } else if mode == 2 {
                        due = Instant::now();
                    }
                }
                thread::sleep(Duration::from_micros(200));
            }
        });
        Self {
            port,
            samples,
            mode,
            stop,
            sessions,
            queries,
            worker: Some(worker),
        }
    }
    fn generations(&self) -> Vec<Session> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.starts > 0)
            .cloned()
            .collect()
    }
}
impl Drop for Radio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct Daemon {
    child: Child,
    output: Arc<Mutex<Vec<serde_json::Value>>>,
    errors: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
}
impl Daemon {
    fn start(path: &Path) -> Self {
        let mut child = manta()
            .args(["run", "--json", "--config"])
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(String::new()));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = output.clone();
        let err = errors.clone();
        let readers = vec![
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        if let Some(spot) = value.get("spot") {
                            out.lock().unwrap().push(spot.clone());
                        }
                    }
                }
            }),
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    *err.lock().unwrap() += &(line + "\n");
                }
            }),
        ];
        Self {
            child,
            output,
            errors,
            readers,
        }
    }
    fn ready(&self) {
        wait_until(
            Duration::from_secs(15),
            || self.errors.lock().unwrap().contains("manta: listening on"),
            || self.errors.lock().unwrap().clone(),
        );
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}
fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool, why: impl Fn() -> String) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "deadline: {}", why());
        thread::sleep(Duration::from_millis(30));
    }
}
fn request(port: u16, path: &str) -> String {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    ) else {
        return String::new();
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let _ = write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}
fn wait_for_health(port: u16, expected: &[(&str, bool)], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let health = request(port, "/healthz");
        if expected.iter().all(|(name, up)| {
            health.contains(&format!(
                "source {name}: {}",
                if *up { "healthy" } else { "unhealthy" }
            ))
        }) {
            return;
        }
        assert!(Instant::now() < deadline, "health deadline: {health}");
        // The production HTTP listener permits 60 requests/minute per IP.
        thread::sleep(Duration::from_millis(1200));
    }
}

fn run_output(path: &Path, verb: &[&str]) -> Output {
    // Reap even if the deadline assertion fails.
    let mut child = manta()
        .args(verb)
        .arg("--config")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "startup did not finish: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn assert_allocation(session: &Session, frequencies: &[u32]) {
    assert_eq!(session.count, Some(frequencies.len() as u8), "{session:?}");
    assert_eq!(
        session.frequencies,
        frequencies
            .iter()
            .enumerate()
            .map(|(i, &f)| (i as u8, f))
            .collect()
    );
}

#[test]
fn four_bands_tune_four_receivers_and_share_stdout_json_and_health() {
    let radio = Radio::start(Some(4), Some(fixture()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manta.toml");
    let ports = config(&path, radio.port, 4, None, false);
    let daemon = Daemon::start(&path);
    daemon.ready();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let metrics = request(ports[2], "/metrics");
        let malformed = metrics
            .lines()
            .find_map(|line| {
                line.strip_prefix("manta_input_malformed_packets_total{source=\"80m\"} ")
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0);
        if malformed > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "malformed packets must be counted before the health assertion"
        );
        thread::sleep(Duration::from_millis(1200));
    }
    let health = request(ports[2], "/healthz");
    for name in NAMES {
        assert!(
            health.contains(&format!("source {name}: unhealthy")),
            "{health}"
        );
    }
    let stream = TcpStream::connect(("127.0.0.1", ports[1])).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let json_stop = Arc::new(AtomicBool::new(false));
    let stop = json_stop.clone();
    let (tx, rx) = mpsc::channel();
    let json = thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        while !stop.load(Ordering::Relaxed) {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        let _ = tx.send(value);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
    });
    // A guard joins the listener reader even if an assertion below unwinds.
    struct ReaderGuard(Arc<AtomicBool>, Option<JoinHandle<()>>);
    impl Drop for ReaderGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
            if let Some(t) = self.1.take() {
                let _ = t.join();
            }
        }
    }
    let _reader = ReaderGuard(json_stop, Some(json));
    radio.mode.store(1, Ordering::Relaxed);
    wait_until(
        Duration::from_secs(80),
        || {
            let spots = daemon.output.lock().unwrap();
            CALLS
                .iter()
                .all(|call| spots.iter().any(|s| s["dxCall"] == *call))
        },
        || {
            format!(
                "spots {:?}; {}",
                daemon.output.lock().unwrap(),
                daemon.errors.lock().unwrap()
            )
        },
    );
    for (i, call) in CALLS.iter().enumerate() {
        let spots = daemon.output.lock().unwrap();
        let spot = spots.iter().find(|s| s["dxCall"] == *call).unwrap();
        assert!(
            (spot["frequency"].as_f64().unwrap() - (f64::from(FREQUENCIES[i]) + 12340.0)).abs()
                < 100.0,
            "{spot}"
        );
    }
    let mut shared = BTreeSet::new();
    wait_until(
        Duration::from_secs(10),
        || {
            while let Ok(value) = rx.try_recv() {
                if let Some(call) = value["dxCall"].as_str() {
                    shared.insert(call.to_string());
                }
            }
            CALLS.iter().all(|call| shared.contains(*call))
        },
        || "shared JSON listener missed a band".into(),
    );
    let body = request(ports[2], "/metrics");
    for name in NAMES {
        assert!(
            body.contains(&format!("manta_source_health{{source=\"{name}\"}} 1")),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "manta_input_queue_dropped_samples_total{{source=\"{name}\"}} "
            )),
            "{body}"
        );
    }
    let health = request(ports[2], "/healthz");
    for name in NAMES {
        assert!(
            health.contains(&format!("source {name}: healthy")),
            "{health}"
        );
    }
    let sessions = radio.generations();
    assert_eq!(sessions.len(), 1);
    assert_allocation(&sessions[0], &FREQUENCIES);
    assert_eq!(*radio.queries.lock().unwrap(), 1);
}

#[test]
fn six_bands_on_four_hardware_receivers_refuse_before_control_or_listeners() {
    for declaration in [None, Some(6)] {
        let radio = Radio::start(Some(4), None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("six.toml");
        let ports = config(&path, radio.port, 6, declaration, false);
        let output = run_output(&path, &["run"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.lines().any(|l| l == format!("error: input \"80m\" failed to open: HPSDR radio at 127.0.0.1:{} has 4 receivers; 6 bands are configured", radio.port)), "{error}");
        assert!(radio.sessions.lock().unwrap().is_empty());
        for port in ports {
            assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
        }
    }
}

#[test]
fn declared_capacity_refuses_offline_and_unknown_capacity_requires_fallback() {
    let radio = Radio::start(None, None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("six.toml");
    config(&path, radio.port, 6, Some(4), false);
    let output = run_output(&path, &["config", "check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("declares 4 receivers; 6 bands are configured"));
    assert_eq!(*radio.queries.lock().unwrap(), 0);
    config(&path, radio.port, 4, None, false);
    let output = run_output(&path, &["run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("cannot determine receiver capacity for HPSDR radio at 127.0.0.1:{}; set receivers on input \"80m\" to the count supported by its gateware", radio.port)));
    assert!(radio.sessions.lock().unwrap().is_empty());
    config(&path, radio.port, 4, Some(4), false);
    let daemon = Daemon::start(&path);
    daemon.ready();
    assert_allocation(&radio.generations()[0], &FREQUENCIES);
}

#[test]
fn aliases_are_rejected_before_any_radio_control() {
    let radio = Radio::start(Some(4), None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    config(&path, radio.port, 2, None, false);
    let text = std::fs::read_to_string(&path).unwrap();
    let (first, second) = text.split_once("name = \"40m\"").unwrap();
    std::fs::write(
        &path,
        format!(
            "{first}name = \"40m\"{}",
            second.replace("127.0.0.1", "localhost")
        ),
    )
    .unwrap();
    let output = run_output(&path, &["run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("HPSDR inputs \"80m\" and \"40m\" resolve to the same radio; use the same host and port for all its bands"));
    assert!(radio.sessions.lock().unwrap().is_empty());
}

#[test]
fn selected_check_uses_receiver_zero_and_does_not_probe() {
    let radio = Radio::start(Some(4), Some(fixture()));
    radio.mode.store(1, Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("four.toml");
    config(&path, radio.port, 4, Some(4), false);
    let output = run_output(&path, &["check", "--input", "20m", "--duration", "1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(*radio.queries.lock().unwrap(), 0);
    assert_allocation(&radio.generations()[0], &[FREQUENCIES[2]]);
}

#[test]
fn a_radio_outage_preserves_other_radio_output_and_restores_transformed_receivers() {
    let radio = Radio::start(Some(4), Some(fixture()));
    // The independent radio uses a distinct call. It starts with malformed
    // data, then supplies real IQ during the first radio's outage.
    let independent = Radio::start(Some(4), Some(Arc::new(vec![fixture()[3].clone()])));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.toml");
    let ports = config(&path, radio.port, 2, None, true);
    let text = std::fs::read_to_string(&path).unwrap() + &format!("\n[[input]]\nname = \"independent\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\nport = {}\nfreq_hz = 28025000.0\nrate_hz = 192000.0\n", independent.port);
    std::fs::write(&path, text).unwrap();
    let daemon = Daemon::start(&path);
    daemon.ready();
    radio.mode.store(1, Ordering::Relaxed);
    wait_until(
        Duration::from_secs(70),
        || {
            daemon
                .output
                .lock()
                .unwrap()
                .iter()
                .any(|s| s["dxCall"] == "JA1ABC")
        },
        || daemon.errors.lock().unwrap().clone(),
    );
    let spot = daemon
        .output
        .lock()
        .unwrap()
        .iter()
        .find(|s| s["dxCall"] == "JA1ABC")
        .unwrap()
        .clone();
    assert!(
        (spot["frequency"].as_f64().unwrap() - 7112340.0 * 1.0001).abs() < 100.0,
        "{spot}"
    );
    assert_eq!(radio.generations().len(), 1);
    radio.mode.store(2, Ordering::Relaxed);
    wait_for_health(
        ports[2],
        &[("80m", false), ("40m", false)],
        Duration::from_secs(20),
    );
    independent.mode.store(1, Ordering::Relaxed);
    wait_until(
        Duration::from_secs(70),
        || {
            daemon
                .output
                .lock()
                .unwrap()
                .iter()
                .any(|s| s["dxCall"] == "VK2ABC")
        },
        || daemon.errors.lock().unwrap().clone(),
    );
    let health = request(ports[2], "/healthz");
    assert!(health.contains("source independent: healthy"), "{health}");
    assert!(health.contains("source 80m: unhealthy"), "{health}");
    *radio.samples.lock().unwrap() =
        Some(Arc::new(vec![fixture()[0].clone(), fixture()[2].clone()]));
    radio.mode.store(1, Ordering::Relaxed);
    wait_for_health(
        ports[2],
        &[("80m", true), ("40m", true)],
        Duration::from_secs(30),
    );
    wait_until(
        Duration::from_secs(70),
        || {
            daemon
                .output
                .lock()
                .unwrap()
                .iter()
                .any(|s| s["dxCall"] == "DL1ABC")
        },
        || daemon.errors.lock().unwrap().clone(),
    );
    let spot = daemon
        .output
        .lock()
        .unwrap()
        .iter()
        .find(|s| s["dxCall"] == "DL1ABC")
        .unwrap()
        .clone();
    assert!(
        (spot["frequency"].as_f64().unwrap() - 7112340.0 * 1.0001).abs() < 100.0,
        "{spot}"
    );
    let sessions = radio.generations();
    assert!(sessions.len() >= 2);
    for session in &sessions {
        assert_allocation(session, &FREQUENCIES[..2]);
    }
    assert_eq!(
        *radio.queries.lock().unwrap(),
        1,
        "capacity is only queried initially"
    );
    assert_eq!(independent.generations().len(), 1);
    assert!(daemon.errors.lock().unwrap().contains("input 40m: back"));
}

#[test]
fn timed_out_capacity_without_a_declaration_refuses_without_starting() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent.toml");
    config(&path, socket.local_addr().unwrap().port(), 4, None, false);
    let output = run_output(&path, &["run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot determine receiver capacity"));
    let mut datagram = [0; 65536];
    let (n, _) = socket.recv_from(&mut datagram).unwrap();
    assert_eq!(n, 60);
    assert_eq!(&datagram[..3], &[0xef, 0xfe, 2]);
    assert!(socket.recv_from(&mut datagram).is_err());
}

#[test]
fn failed_startup_retires_another_groups_completed_radio() {
    let opened = Radio::start(Some(4), None);
    let unknown = Radio::start(None, None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.toml");
    config(&path, opened.port, 2, None, false);
    let mut text = std::fs::read_to_string(&path).unwrap();
    for name in ["other1", "other2"] {
        text += &format!("\n[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\nport = {}\nfreq_hz = 21025000.0\nrate_hz = 192000.0\n", unknown.port);
    }
    std::fs::write(&path, text).unwrap();
    let output = run_output(&path, &["run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("input \"other1\" failed to open"));
    assert!(unknown.sessions.lock().unwrap().is_empty());
    wait_until(
        Duration::from_secs(2),
        || opened.generations().iter().any(|s| s.stops == 1),
        || format!("{:?}", opened.generations()),
    );
    assert_eq!(opened.generations().len(), 1);
}
