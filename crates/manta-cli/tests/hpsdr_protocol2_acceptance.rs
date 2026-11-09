//! MAN-305 acceptance: manta streams IQ from an openHPSDR protocol-2
//! (ANAN-class) radio, picks protocol 2 on its own when the radio answers
//! discovery as protocol 2, and reports lost datagrams.
//!
//! No hardware is used (MAN-52 owns the real-radio run): a fake protocol-2
//! radio in this process binds `base..=base+3` and `base+11` on loopback,
//! answers discovery as board 5 (Orion II), and once manta sends
//! high-priority with run = 1 it streams a V1 fixture's samples in 238-pair
//! datagrams from `base+11`, paced at real time. A real `manta` child
//! is pointed at `--hpsdr-port <base>`, with no `--hpsdr-protocol`.

#[path = "common/launch.rs"]
mod launch;
use launch::{free_ports, manta};

use manta_input::IqSource;
use num_complex::Complex32;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RATE: f64 = 192_000.0;
const READY_MARKER: &str = "manta: listening;";
/// IQ pairs per DDC datagram (MAN-305 D9).
const PER_DATAGRAM: usize = 238;
/// A DDC datagram's length: 16-byte header, then 238 pairs of 24-bit I, Q.
const IQ_DATAGRAM_LEN: usize = 16 + 6 * PER_DATAGRAM;
/// The high-priority packet's length (D6).
const HIGH_PRIORITY_LEN: usize = 1444;
/// Protocol 2's 24-bit full scale (D9).
const FULL_SCALE: f32 = 8_388_608.0;
/// One channelizer hop: how far a spot's `sample_ts` may move (plan,
/// Desired End State).
const SAMPLE_TS_TOLERANCE: i64 = 512;
/// How far a spot's frequency may move from the file replay's. Any splice
/// inside the track moves the estimate (measured 1.6-13 Hz in the cloud
/// container, where the kernel itself dropped 10-213 datagrams at
/// startup); a quarter of RBN's 100 Hz reporting step.
const FREQ_TOLERANCE_HZ: f64 = 25.0;

/// V1 at 192 kHz, shortened to 30 s (centre 14 MHz).
fn v1_fixture(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        fs: RATE,
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

fn wav_samples(path: &Path) -> Vec<Complex32> {
    let mut src = manta_input::WavIqSource::open(path).unwrap();
    assert_eq!(src.sample_rate(), RATE);
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

/// A spot's `dxCall`, absolute `frequency` and `sampleTs`.
type Spot = (String, f64, u64);

fn spot_of(v: &serde_json::Value) -> Spot {
    (
        v["dxCall"].as_str().unwrap().to_string(),
        v["frequency"].as_f64().unwrap(),
        v["sampleTs"].as_u64().unwrap(),
    )
}

/// The single spot of `manta run --source <wav> --source-iq --json`.
fn file_replay_spot(wav: &Path) -> Spot {
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
    let spots: Vec<Spot> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("spot").map(spot_of))
        .collect();
    assert_eq!(spots.len(), 1, "file replay spots: {spots:?}");
    spots[0].clone()
}

fn is_discovery_request(d: &[u8]) -> bool {
    (d.len() == 60 && d[4] == 0x02 && d.iter().enumerate().all(|(i, &b)| i == 4 || b == 0))
        || (d.len() == 63 && d[..3] == [0xEF, 0xFE, 0x02])
}

/// The protocol-2 discovery reply of an idle Orion II (board 5, protocol
/// version 39, firmware 21, one DDC).
fn orion2_reply() -> Vec<u8> {
    let mut r = vec![0u8; 60];
    r[4] = 0x02;
    r[5..11].copy_from_slice(&[0x00, 0x1C, 0xC0, 0xA2, 0x13, 0x5D]);
    r[11] = 5;
    r[12] = 39;
    r[13] = 21;
    r[20] = 1;
    r
}

fn put_i24(p: &mut [u8], at: usize, x: f32) {
    let v = (x * FULL_SCALE)
        .round()
        .clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i32;
    p[at..at + 3].copy_from_slice(&v.to_be_bytes()[1..4]);
}

/// A DDC0 datagram numbered `seq` carrying `pairs` (238 of them; a short
/// final block is zero-padded).
fn iq_datagram(seq: u32, pairs: &[Complex32]) -> Vec<u8> {
    let mut p = vec![0u8; IQ_DATAGRAM_LEN];
    p[0..4].copy_from_slice(&seq.to_be_bytes());
    p[12..14].copy_from_slice(&24u16.to_be_bytes());
    p[14..16].copy_from_slice(&(PER_DATAGRAM as u16).to_be_bytes());
    for (k, s) in pairs.iter().enumerate() {
        put_i24(&mut p, 16 + 6 * k, s.re);
        put_i24(&mut p, 16 + 6 * k + 3, s.im);
    }
    p
}

/// How the fake streams once manta sends run = 1.
struct PauseGates {
    blocks: Vec<usize>,
    stopped: mpsc::Sender<Instant>,
    resume: mpsc::Receiver<()>,
}

struct Stream {
    gates: Option<PauseGates>,
    samples: Arc<Vec<Complex32>>,
    speed: f64,
    /// `(first, count)`: sequence numbers `first..first + count` and their
    /// samples are never sent.
    skip: Option<(u32, u32)>,
    looping: bool,
    /// The first socket manta streams from gets only this many datagrams,
    /// then silence, so manta stalls and reconnects from a new one.
    first_connection_datagrams: Option<u32>,
    /// Streaming waits for this too: set once manta is reading, as the
    /// ka9q acceptance test waits for the ready line, so a startup backlog
    /// cannot overflow the socket buffer.
    go: Arc<AtomicBool>,
}

/// Where the fake streams: manta's IQ socket as its run commands say.
#[derive(Default)]
struct Peer {
    /// The latest run = 1 sender, cleared by that sender's run = 0.
    current: Option<SocketAddr>,
    /// Counts run = 1 senders, 1 for the first: a socket that starts
    /// streaming again after its run = 0 counts anew, so a reconnect that
    /// reuses the dead socket's port still gets a fresh pass.
    connection: u32,
}

/// A loopback protocol-2 radio: discovery on `base`, bring-up on
/// `base..=base+3`, DDC0 IQ from `base+11`. It streams to the latest
/// socket that sent run = 1, as a radio does, so it follows manta across
/// a reconnect; a new socket gets a fresh pass from block 0.
struct FakeP2Radio {
    base: u16,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl FakeP2Radio {
    fn start(stream: Stream, done: mpsc::Sender<()>) -> Self {
        Self::start_on([127, 0, 0, 1], stream, done)
    }

    /// As [`Self::start`], bound on `ip`. Hostless discovery includes a
    /// loopback-unicast destination to reach local fakes (MAN-306).
    fn start_on(ip: [u8; 4], mut stream: Stream, done: mpsc::Sender<()>) -> Self {
        let (base, cmd, iq) = Self::bind(ip);
        let stop = Arc::new(AtomicBool::new(false));
        let peer = Arc::new(Mutex::new(Peer::default()));
        let mut threads = Vec::new();

        // `base`: answer every discovery request (each reopen asks again).
        let general = cmd[0].try_clone().unwrap();
        let s = stop.clone();
        threads.push(thread::spawn(move || {
            general
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            let mut buf = vec![0u8; 65536];
            while !s.load(Ordering::SeqCst) {
                if let Ok((n, from)) = general.recv_from(&mut buf) {
                    if is_discovery_request(&buf[..n]) {
                        let _ = general.send_to(&orion2_reply(), from);
                    }
                }
            }
        }));

        // `base+3`: follow run = 1 to manta's latest IQ socket, and stop on
        // that socket's run = 0 (a late one from a dead socket is ignored).
        let high_priority = cmd[3].try_clone().unwrap();
        let (s, p) = (stop.clone(), peer.clone());
        threads.push(thread::spawn(move || {
            high_priority
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            let mut buf = vec![0u8; 65536];
            while !s.load(Ordering::SeqCst) {
                if let Ok((n, from)) = high_priority.recv_from(&mut buf) {
                    if n == HIGH_PRIORITY_LEN {
                        let mut p = p.lock().unwrap();
                        if buf[4] & 1 == 1 {
                            if p.current != Some(from) {
                                p.current = Some(from);
                                p.connection += 1;
                            }
                        } else if p.current == Some(from) {
                            p.current = None;
                        }
                    }
                }
            }
        }));

        // `base+11`: stream to the current run = 1 sender, paced by the
        // clock. A new sender starts a fresh pass from block 0; a finished
        // or cut pass is not resent to the same socket.
        let s = stop.clone();
        threads.push(thread::spawn(move || {
            let blocks = stream.samples.len().div_ceil(PER_DATAGRAM);
            let per_block = Duration::from_secs_f64(PER_DATAGRAM as f64 / (RATE * stream.speed));
            let (mut streaming_to, mut t0, mut seq) = (None, Instant::now(), 0u32);
            let mut finished: Option<u32> = None;
            let mut last_send = Instant::now();
            while !s.load(Ordering::SeqCst) {
                let target = {
                    let p = peer.lock().unwrap();
                    p.current
                        .map(|to| (to, p.connection))
                        .filter(|&(_, c)| stream.go.load(Ordering::SeqCst) && finished != Some(c))
                };
                let Some((to, connection)) = target else {
                    streaming_to = None;
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                if streaming_to != Some(connection) {
                    (streaming_to, t0, seq) = (Some(connection), Instant::now(), 0);
                }
                let due = t0 + per_block * seq;
                let now = Instant::now();
                if now < due {
                    thread::sleep((due - now).min(Duration::from_millis(1)));
                    continue;
                }
                let block = seq as usize;
                if let Some(gates) = stream.gates.as_mut() {
                    if gates.blocks.first() == Some(&block) {
                        gates.blocks.remove(0);
                        let _ = gates.stopped.send(last_send);
                        loop {
                            match gates.resume.recv_timeout(Duration::from_millis(20)) {
                                Ok(()) => break,
                                Err(mpsc::RecvTimeoutError::Timeout)
                                    if !s.load(Ordering::SeqCst) =>
                                {
                                    continue
                                }
                                _ => return,
                            }
                        }
                        t0 = Instant::now() - per_block * seq;
                    }
                }
                let cut = connection == 1 && stream.first_connection_datagrams == Some(seq);
                if cut || (!stream.looping && block >= blocks) {
                    (finished, streaming_to) = (Some(connection), None);
                    if !cut {
                        let _ = done.send(());
                    }
                    continue;
                }
                let block = block % blocks;
                let skipped = stream
                    .skip
                    .is_some_and(|(first, count)| seq >= first && seq < first + count);
                if !skipped {
                    let start = block * PER_DATAGRAM;
                    let end = (start + PER_DATAGRAM).min(stream.samples.len());
                    iq.send_to(&iq_datagram(seq, &stream.samples[start..end]), to)
                        .unwrap();
                    last_send = Instant::now();
                }
                seq = seq.wrapping_add(1);
            }
        }));

        // `base+1` and `base+2` stay bound (in `cmd`, moved into this
        // thread) so manta's rx/tx-specific packets have somewhere to go.
        let s = stop.clone();
        threads.push(thread::spawn(move || {
            let _cmd = cmd;
            while !s.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
        }));

        Self {
            base,
            stop,
            threads,
        }
    }

    /// Bind `base..=base+3` and `base+11` on `ip` for some free base.
    fn bind(ip: [u8; 4]) -> (u16, Vec<UdpSocket>, UdpSocket) {
        let ip = std::net::Ipv4Addr::from(ip);
        for _ in 0..200 {
            let first = UdpSocket::bind((ip, 0)).unwrap();
            let base = first.local_addr().unwrap().port();
            if base > u16::MAX - 11 {
                continue;
            }
            let rest: Option<Vec<UdpSocket>> = (1..=3)
                .map(|o| UdpSocket::bind((ip, base + o)).ok())
                .collect();
            let Some(rest) = rest else { continue };
            let Ok(iq) = UdpSocket::bind((ip, base + 11)) else {
                continue;
            };
            let mut cmd = vec![first];
            cmd.extend(rest);
            return (base, cmd, iq);
        }
        panic!("no free base port for the fake protocol-2 radio");
    }
}

impl Drop for FakeP2Radio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// What one `manta run` against the fake produced.
struct LiveRun {
    /// Every spot manta published while the fake streamed.
    spots: Vec<Spot>,
    metrics: String,
    stderr: String,
}

impl LiveRun {
    fn explain(&self) -> String {
        explain(&self.stderr, &self.metrics)
    }
}

/// What a failing live-run assertion prints: how often manta lost and
/// regained the source, then its stderr and a `/metrics` body.
fn explain(stderr: &str, metrics: &str) -> String {
    let lost = stderr.lines().filter(|l| l.contains(" lost: ")).count();
    let reconnected = stderr
        .lines()
        .filter(|l| l.contains(" reconnected after "))
        .count();
    format!(
        "source lost {lost} time(s), reconnected {reconnected} time(s)\nstderr:\n{stderr}\nmetrics:\n{metrics}"
    )
}

/// Run `manta run --hpsdr-host 127.0.0.1 --hpsdr-port <base> ... --json`
/// (no `--hpsdr-protocol`) against a fake streaming `samples` at real time,
/// skipping `skip` and cutting the first connection after
/// `first_connection_datagrams`. Returns every spot, a `/metrics` scrape
/// taken once the stream has ended and `ready(metrics)` holds (or 10 s
/// passed), and stderr.
///
/// Real-time pacing leaves CPU-starved runners room to drain the OS-capped
/// receive buffer (about 425 KB on Linux despite the 4 MiB request).
/// Accelerated finite streams can lose the first-pass W1AW decode under runner
/// load, triggering a reconnect and a second-pass timestamp shift (MAN-497, MAN-505).
fn live_run(
    dir: &Path,
    samples: Arc<Vec<Complex32>>,
    skip: Option<(u32, u32)>,
    first_connection_datagrams: Option<u32>,
    ready: impl Fn(&str) -> bool,
) -> LiveRun {
    let (done_tx, done_rx) = mpsc::channel();
    let go = Arc::new(AtomicBool::new(false));
    let fake = FakeP2Radio::start(
        Stream {
            gates: None,
            samples,
            speed: 1.0,
            skip,
            looping: false,
            first_connection_datagrams,
            go: go.clone(),
        },
        done_tx,
    );
    let [telnet_port, json_port, metrics_port] = free_ports();
    let cfg = dir.join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\n"
        ),
    )
    .unwrap();
    let mut child = manta()
        .args(["run", "--hpsdr-host", "127.0.0.1", "--hpsdr-port"])
        .arg(fake.base.to_string())
        .args([
            "--hpsdr-freq-hz",
            "14000000",
            "--hpsdr-rate-hz",
            "192000",
            "--json",
            "--config",
        ])
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _guard = ChildGuard(child);

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
                    let _ = spot_tx.send(spot_of(s));
                }
            }
        }
    });
    let stderr_now = || err_lines.lock().unwrap().join("\n");
    let explain_now = || {
        explain(
            &stderr_now(),
            &scrape_metrics(metrics_port).unwrap_or_default(),
        )
    };
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("manta never printed {READY_MARKER:?}; {}", explain_now()));
    go.store(true, Ordering::SeqCst);
    let spot = spot_rx
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("no spot within 60 s; {}", explain_now()));
    done_rx
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("the fake never finished streaming; {}", explain_now()));
    // Let the last datagrams and a metrics tick land.
    let deadline = Instant::now() + Duration::from_secs(10);
    let metrics = loop {
        let body = scrape_metrics(metrics_port).unwrap_or_default();
        if ready(&body) || Instant::now() >= deadline {
            break body;
        }
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = stderr_now();
    drop(fake);
    let mut spots = vec![spot];
    spots.extend(spot_rx.try_iter());
    LiveRun {
        spots,
        metrics,
        stderr,
    }
}

/// The run published exactly one spot, the file replay's `W1AW`, at its
/// frequency and `sample_ts`. Every message carries `run.explain()`.
fn assert_same_spot(run: &LiveRun, file: &Spot) {
    assert_eq!(file.0, "W1AW");
    assert_eq!(
        run.spots.len(),
        1,
        "spots: {:?}; {}",
        run.spots,
        run.explain()
    );
    let live = &run.spots[0];
    assert_eq!(live.0, file.0, "{}", run.explain());
    assert!(
        (live.1 - file.1).abs() <= FREQ_TOLERANCE_HZ,
        "protocol-2 spot at {} Hz, file replay at {} Hz; {}",
        live.1,
        file.1,
        run.explain()
    );
    let off = live.2 as i64 - file.2 as i64;
    assert!(
        off.abs() <= SAMPLE_TS_TOLERANCE,
        "protocol-2 spot at sample_ts {}, file replay at {} ({off:+}); {}",
        live.2,
        file.2,
        run.explain()
    );
}

/// Gherkin (MAN-305):
///   Given an ANAN-class radio speaking openHPSDR protocol 2 on the LAN
///   When manta's network-radio input is configured for that radio at 192 kHz
///   Then manta decodes and spots CW signals from its stream
/// and
///   Given a radio that answers discovery as protocol 2
///   When manta connects without a protocol being specified
///   Then manta uses protocol 2
#[test]
fn protocol2_radio_at_192k_spots_w1aw_like_the_file_replay() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let file = file_replay_spot(&wav);
    let samples = Arc::new(wav_samples(&wav));
    let health = r#"manta_source_health{source="hpsdr"}"#;
    let run = live_run(dir.path(), samples, None, None, |m| {
        metric_value(m, health) == Some(1)
    });

    assert!(
        run.stderr.contains(
            "answered discovery as openHPSDR protocol 2 (board 5, Orion II; protocol version 39; \
             firmware 21); connecting with protocol 2"
        ),
        "{}",
        run.explain()
    );
    assert_same_spot(&run, &file);
    assert_eq!(
        metric_value(&run.metrics, health),
        Some(1),
        "{}",
        run.explain()
    );
    // Exposed for the radio. Not pinned at 0: a CI kernel can drop
    // datagrams for real (this container dropped 10-213 at startup), and
    // the `sample_ts` check above already shows each was reported.
    assert!(
        metric_value(
            &run.metrics,
            r#"manta_input_dropped_packets_total{source="hpsdr"}"#
        )
        .is_some(),
        "{}",
        run.explain()
    );
}

/// Gherkin (MAN-305):
///   Given manta is streaming from a protocol-2 radio
///   When 50 datagrams are lost
///   Then the input-loss metrics increase by 50
///   And the gap is reported to the decoder as missing input
///
/// The exact +50 is pinned at library level
/// (`fifty_lost_datagrams_count_fifty_and_report_11900_missing_samples`);
/// here `>= 50`, because a CI kernel can drop a datagram for real. The
/// `sample_ts` check holds either way: every drop is reported, and an
/// unreported 50-datagram splice would put the spot ~11,776 samples early.
#[test]
fn fifty_lost_datagrams_are_counted_and_keep_the_spot_clock_true() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let file = file_replay_spot(&wav);
    let samples = Arc::new(wav_samples(&wav));
    // The datagram that starts at 3.0 s, and the 49 after it.
    let first = (3.0 * RATE / PER_DATAGRAM as f64) as u32;
    let dropped = r#"manta_input_dropped_packets_total{source="hpsdr"}"#;
    let gaps = r#"manta_input_gaps_detected_total{source="hpsdr"}"#;
    let run = live_run(dir.path(), samples, Some((first, 50)), None, |m| {
        metric_value(m, dropped).unwrap_or(0) >= 50 && metric_value(m, gaps).unwrap_or(0) >= 1
    });

    assert!(
        metric_value(&run.metrics, dropped).unwrap_or(0) >= 50,
        "{}",
        run.explain()
    );
    assert!(
        metric_value(&run.metrics, gaps).unwrap_or(0) >= 1,
        "{}",
        run.explain()
    );
    assert_same_spot(&run, &file);
}

/// The first connection gets this many datagrams before the fake cuts it:
/// 1 s of content at 192 kS/s (192,000 / 238, rounded up), so manta's
/// first connection is productive and its stall reads as a disconnect.
const FIRST_CONNECTION_DATAGRAMS: u32 = 807;

/// Gherkin (MAN-459):
///   Given a protocol-2 acceptance test whose manta stalls and reconnects on a new local port
///   When the reconnected manta sends its run command
///   Then the fake radio streams IQ to the new port
///   And the test spots W1AW within its deadline
///
/// The spot clock moves by the cut pass's samples plus the outage manta
/// reports (MAN-73), within the note's 0.1 s rounding.
#[test]
fn the_fake_radio_follows_manta_across_a_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let file = file_replay_spot(&wav);
    let samples = Arc::new(wav_samples(&wav));
    let health = r#"manta_source_health{source="hpsdr"}"#;
    let run = live_run(
        dir.path(),
        samples,
        None,
        Some(FIRST_CONNECTION_DATAGRAMS),
        |m| metric_value(m, health) == Some(1),
    );

    for want in ["stalled: no IQ datagram for 10 s", "reconnecting in 1s"] {
        assert!(
            run.stderr.contains(want),
            "missing {want:?}; {}",
            run.explain()
        );
    }
    let outage_s: f64 = run
        .stderr
        .lines()
        .find_map(|l| {
            l.split_once("source hpsdr reconnected after ")
                .map(|(_, t)| t)
        })
        .and_then(|t| t.trim().strip_suffix('s'))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no reconnect note found; {}", run.explain()));
    assert_eq!(
        run.spots.len(),
        1,
        "spots: {:?}; {}",
        run.spots,
        run.explain()
    );
    let live = &run.spots[0];
    assert_eq!(live.0, "W1AW", "{}", run.explain());
    assert!(
        (live.1 - file.1).abs() <= FREQ_TOLERANCE_HZ,
        "protocol-2 spot at {} Hz, file replay at {} Hz; {}",
        live.1,
        file.1,
        run.explain()
    );
    let cut = FIRST_CONNECTION_DATAGRAMS as f64 * PER_DATAGRAM as f64;
    let shift = live.2 as f64 - file.2 as f64;
    let off = shift - cut - outage_s * RATE;
    assert!(
        off.abs() <= 0.05 * RATE + SAMPLE_TS_TOLERANCE as f64,
        "protocol-2 spot at sample_ts {}, file replay at {}: shift {shift:+} is {off:+} from \
         {cut} cut samples plus a {outage_s} s outage; {}",
        live.2,
        file.2,
        run.explain()
    );
}

#[test]
fn check_reports_a_protocol2_radio() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let mut samples = wav_samples(&wav);
    samples.truncate(3 * RATE as usize);
    let (done_tx, _done_rx) = mpsc::channel();
    let fake = FakeP2Radio::start(
        Stream {
            gates: None,
            samples: Arc::new(samples),
            speed: 1.0,
            skip: None,
            looping: true,
            first_connection_datagrams: None,
            go: Arc::new(AtomicBool::new(true)),
        },
        done_tx,
    );
    let out = manta()
        .args(["check", "--hpsdr-host", "127.0.0.1", "--hpsdr-port"])
        .arg(fake.base.to_string())
        .args([
            "--hpsdr-freq-hz",
            "14000000",
            "--hpsdr-rate-hz",
            "192000",
            "--duration",
            "1",
        ])
        .output()
        .unwrap();
    drop(fake);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    for want in [
        "Source: hpsdr",
        "Stream sample rate: 192000 Hz",
        "Center frequency: 14000000 Hz",
    ] {
        assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}");
    }
    assert!(
        stderr.contains("answered discovery as openHPSDR protocol 2 (board 5, Orion II"),
        "{stderr}"
    );
}

/// MAN-306: with no address, discovery finds the protocol-2 radio and the
/// MAN-305 driver opens it, exactly as with an explicit host.
#[test]
fn check_discovers_a_protocol2_radio_without_an_address() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let mut samples = wav_samples(&wav);
    samples.truncate(3 * RATE as usize);
    let (done_tx, _done_rx) = mpsc::channel();
    let fake = FakeP2Radio::start_on(
        [127, 0, 0, 1],
        Stream {
            gates: None,
            samples: Arc::new(samples),
            speed: 1.0,
            skip: None,
            looping: true,
            first_connection_datagrams: None,
            go: Arc::new(AtomicBool::new(true)),
        },
        done_tx,
    );
    let out = manta()
        .args(["check", "--hpsdr-port"])
        .arg(fake.base.to_string())
        .args([
            "--hpsdr-freq-hz",
            "14000000",
            "--hpsdr-rate-hz",
            "192000",
            "--duration",
            "1",
        ])
        .output()
        .unwrap();
    drop(fake);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("Stream sample rate: 192000 Hz"), "{stdout}");
    assert!(
        stderr.contains("answered discovery as openHPSDR protocol 2 (board 5, Orion II"),
        "{stderr}"
    );
}

/// MAN-307 x MAN-305: with several `[[input]]` entries every HPSDR radio
/// opens through the shared-radio group, and a radio with one entry keeps
/// MAN-305's protocol choice there: `auto` discovers each protocol-2 radio
/// and streams from it, as the one-input path above does.
#[test]
fn lone_protocol2_radios_in_a_multi_input_run_are_discovered_and_stream() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let samples = Arc::new(wav_samples(&wav));
    let go = Arc::new(AtomicBool::new(false));
    let (done_tx, _done_rx) = mpsc::channel();
    let radios: Vec<FakeP2Radio> = (0..2)
        .map(|_| {
            FakeP2Radio::start(
                Stream {
                    gates: None,
                    samples: samples.clone(),
                    speed: 1.0,
                    skip: None,
                    looping: true,
                    first_connection_datagrams: None,
                    go: go.clone(),
                },
                done_tx.clone(),
            )
        })
        .collect();
    let [telnet_port, json_port, metrics_port] = free_ports();
    let mut text = format!(
        "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\n"
    );
    for (name, radio) in ["a", "b"].iter().zip(&radios) {
        text += &format!(
            "\n[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\nport = {}\nfreq_hz = 14000000.0\nrate_hz = 192000.0\n",
            radio.base
        );
    }
    let cfg = dir.path().join("manta.toml");
    std::fs::write(&cfg, text).unwrap();
    let mut child = manta()
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _guard = ChildGuard(child);
    let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let err_lines = err_lines.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                err_lines.lock().unwrap().push(line);
            }
        });
    }
    let (spot_tx, spot_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(s) = v.get("spot") {
                    let _ = spot_tx.send(spot_of(s));
                }
            }
        }
    });
    let stderr_now = || err_lines.lock().unwrap().join("\n");
    go.store(true, Ordering::SeqCst);
    let spot = spot_rx
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| {
            panic!(
                "no spot within 60 s; {}",
                explain(
                    &stderr_now(),
                    &scrape_metrics(metrics_port).unwrap_or_default()
                )
            )
        });
    let healthy = |m: &str| {
        ["a", "b"].iter().all(|name| {
            metric_value(m, &format!("manta_source_health{{source=\"{name}\"}}")) == Some(1)
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let metrics = loop {
        let body = scrape_metrics(metrics_port).unwrap_or_default();
        if healthy(&body) || Instant::now() >= deadline {
            break body;
        }
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = stderr_now();
    assert_eq!(spot.0, "W1AW", "{}", explain(&stderr, &metrics));
    assert!(healthy(&metrics), "{}", explain(&stderr, &metrics));
    for radio in &radios {
        assert!(
            stderr.contains(&format!(
                "HPSDR radio 127.0.0.1:{} answered discovery as openHPSDR protocol 2 (board 5, \
                 Orion II",
                radio.base
            )),
            "{}",
            explain(&stderr, &metrics)
        );
    }
}

/// MAN-489: no word follows either callsign; the source's soft stall must
/// settle each tail while the fake refuses to send the next sample.
#[test]
fn a_protocol2_held_tail_spots_within_five_seconds_before_same_session_resume() {
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = RATE;
    spec.duration_s = 4.0;
    spec.signals[0].wpm = 40.0;
    spec.signals[0].loop_text = false;
    let mut samples = Vec::new();
    let mut boundaries = Vec::new();
    for text in ["DE W1AW", "DE K1ABC"] {
        spec.signals[0].text = text.into();
        samples.extend(manta_testkit::vectors::render(&spec).unwrap().samples);
        samples.resize(
            samples.len().div_ceil(PER_DATAGRAM) * PER_DATAGRAM,
            Complex32::default(),
        );
        boundaries.push(samples.len() / PER_DATAGRAM);
    }
    let (stopped, pauses) = mpsc::channel();
    let (resume, gate) = mpsc::channel();
    let (done, _) = mpsc::channel();
    let go = Arc::new(AtomicBool::new(false));
    let fake = FakeP2Radio::start(
        Stream {
            gates: Some(PauseGates {
                blocks: boundaries.clone(),
                stopped,
                resume: gate,
            }),
            samples: Arc::new(samples),
            speed: 1.0,
            skip: None,
            looping: false,
            first_connection_datagrams: None,
            go: go.clone(),
        },
        done,
    );
    let dir = tempfile::tempdir().unwrap();
    let [telnet, json, metrics] = free_ports();
    let cfg = dir.path().join("manta.toml");
    std::fs::write(&cfg, format!("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\nmetrics_port = {metrics}\n[spot]\nallowlist = [\"W1AW\", \"K1ABC\"]\n")).unwrap();
    let mut child = manta()
        .args(["run", "--hpsdr-host", "127.0.0.1", "--hpsdr-port"])
        .arg(fake.base.to_string())
        .args([
            "--hpsdr-freq-hz",
            "14000000",
            "--hpsdr-rate-hz",
            "192000",
            "--json",
            "--config",
        ])
        .arg(cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _child = ChildGuard(child);
    let (ready_tx, ready) = mpsc::channel();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let err = errors.clone();
    let stderr_reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains(READY_MARKER) {
                let _ = ready_tx.send(());
            }
            err.lock().unwrap().push(line);
        }
    });
    let (spot_tx, spots) = mpsc::channel();
    let stdout_reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(s) = v.get("spot") {
                    let _ = spot_tx.send((spot_of(s), Instant::now()));
                }
            }
        }
    });
    ready.recv_timeout(Duration::from_secs(30)).unwrap();
    go.store(true, Ordering::SeqCst);
    for (episode, call) in ["W1AW", "K1ABC"].into_iter().enumerate() {
        let last_send = pauses.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(spots.try_recv().is_err(), "held tail spotted before stall");
        let (spot, received) = spots
            .recv_timeout(Duration::from_secs(5).saturating_sub(last_send.elapsed()))
            .unwrap_or_else(|e| panic!("{e}: {:?}", errors.lock().unwrap()));
        assert_eq!(spot.0, call);
        assert!(received.duration_since(last_send) < Duration::from_secs(5));
        let start = if episode == 0 {
            0
        } else {
            boundaries[0] * PER_DATAGRAM
        } as u64;
        let end = (boundaries[episode] * PER_DATAGRAM) as u64;
        assert!(
            spot.2 >= start && spot.2 < end,
            "held word keeps its sample timestamp: {spot:?}"
        );
        assert!(spots.try_recv().is_err());
        assert!(!errors
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(" lost: ") || l.contains(" reconnected after ")));
        resume.send(()).unwrap();
    }
    drop(fake);
    drop(_child);
    stdout_reader.join().unwrap();
    stderr_reader.join().unwrap();
}
