//! MAN-141: manta's spectrum feed against a fake N1MM+ written from N1MM+'s
//! published "Sending Spectrum Data to N1MM+" field list
//! (<https://n1mmwp.hamdocs.com/appendices/external-udp-broadcasts/>), not
//! from manta's formatter: `FakeN1mm::recv` checks only what that page
//! documents. The in-process tests run on every platform; the `manta run`
//! tests are Unix-only, as `relay_acceptance.rs` is.

#[cfg(unix)]
mod common;

use manta_server::spectrum::{self, Frame, SpectrumDrop, SpectrumStats};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Plays N1MM+'s Spectrum Display listener.
struct FakeN1mm {
    socket: UdpSocket,
}

/// One datagram's documented fields.
#[derive(Debug)]
struct Received {
    name: String,
    low_khz: f64,
    high_khz: f64,
    scaling: f64,
    values: Vec<u32>,
}

/// The documented elements, in the order the published example lists them.
const FIELDS: [&str; 7] = [
    "app",
    "Name",
    "LowScopeFrequency",
    "HighScopeFrequency",
    "ScalingFactor",
    "DataCount",
    "SpectrumData",
];

impl FakeN1mm {
    fn bind() -> Self {
        Self::bind_on("127.0.0.1:0".parse().unwrap())
    }

    fn bind_on(addr: SocketAddr) -> Self {
        let socket = UdpSocket::bind(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        FakeN1mm { socket }
    }

    fn addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    /// Receives one datagram and asserts the published protocol: UTF-8, an
    /// XML declaration naming utf-8, a `<Spectrum>` root, the seven fields
    /// in order, Low < High, ScalingFactor > 0, DataCount equal to the
    /// number of comma-separated values, each an integer in 0..=65535.
    fn recv(&self) -> Received {
        self.try_recv().expect("a spectrum datagram within 5 s")
    }

    fn try_recv(&self) -> Option<Received> {
        let mut buf = vec![0u8; 65_536];
        let (len, _) = self.socket.recv_from(&mut buf).ok()?;
        let text = std::str::from_utf8(&buf[..len]).expect("datagram is UTF-8");
        Some(parse(text))
    }
}

/// The text between `<tag>` and `</tag>`, searched from `from`; returns the
/// value and the offset after the closing tag.
fn element<'a>(text: &'a str, tag: &str, from: usize) -> (&'a str, usize) {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text[from..]
        .find(&open)
        .unwrap_or_else(|| panic!("no <{tag}> after offset {from} in {text}"))
        + from
        + open.len();
    let end = text[start..]
        .find(&close)
        .unwrap_or_else(|| panic!("no </{tag}> in {text}"))
        + start;
    (&text[start..end], end + close.len())
}

fn parse(text: &str) -> Received {
    let decl = text.trim_start();
    assert!(decl.starts_with("<?xml"), "no XML declaration: {text}");
    let decl_end = decl.find("?>").expect("closed declaration");
    assert!(
        decl[..decl_end].to_ascii_lowercase().contains("utf-8"),
        "declaration does not name utf-8: {text}"
    );
    let (body, _) = element(decl, "Spectrum", decl_end);
    let mut at = 0;
    let mut values = Vec::new();
    for field in FIELDS {
        let (value, next) = element(body, field, at);
        values.push(value.trim().to_string());
        at = next;
    }
    let low_khz: f64 = values[2].parse().expect("LowScopeFrequency is a number");
    let high_khz: f64 = values[3].parse().expect("HighScopeFrequency is a number");
    let scaling: f64 = values[4].parse().expect("ScalingFactor is a number");
    let count: usize = values[5].parse().expect("DataCount is an integer");
    let data: Vec<u32> = values[6]
        .split(',')
        .map(|v| {
            let n: u32 = v.trim().parse().expect("SpectrumData holds integers");
            assert!(n <= 65_535, "level {n} out of 0..=65535");
            n
        })
        .collect();
    assert!(low_khz < high_khz, "Low {low_khz} >= High {high_khz}");
    assert!(scaling > 0.0, "ScalingFactor {scaling}");
    assert_eq!(count, data.len(), "DataCount vs SpectrumData");
    Received {
        name: values[1].clone(),
        low_khz,
        high_khz,
        scaling,
        values: data,
    }
}

fn frame(low_hz: f64, n: usize) -> Frame {
    Frame {
        low_hz,
        high_hz: low_hz + 93.75 * (n - 1) as f64,
        power_db: (0..n).map(|i| -140.0 + i as f32).collect(),
    }
}

fn spawn_to(
    dest: SocketAddr,
    name: &str,
) -> (
    spectrum::FrameSender,
    std::thread::JoinHandle<()>,
    Arc<SpectrumStats>,
) {
    let socket = UdpSocket::bind(match dest {
        SocketAddr::V4(_) => "127.0.0.1:0",
        SocketAddr::V6(_) => "[::1]:0",
    })
    .unwrap();
    let stats = Arc::new(SpectrumStats::default());
    let (sender, handle) = spectrum::spawn(
        socket,
        dest,
        "test",
        name.to_string(),
        stats.clone(),
        Box::new(|_| {}),
    )
    .unwrap();
    (sender, handle, stats)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_sender_delivers_a_frame_n1mm_accepts() {
    let n1mm = FakeN1mm::bind();
    let (sender, _handle, stats) = spawn_to(n1mm.addr(), "manta hpsdr");
    sender.offer(frame(14_000_000.0, 3));
    let got = n1mm.recv();
    assert_eq!(got.name, "manta hpsdr");
    assert_eq!(got.low_khz, 14_000.0);
    // 14000.1875 kHz, printed with at most three decimals.
    assert!((got.high_khz - 14_000.187_5).abs() < 0.001, "{got:?}");
    assert_eq!(got.scaling, 0.5);
    // -140, -139, -138 dB at 0.5 dB per level from a -140 dB floor.
    assert_eq!(got.values, vec![0, 2, 4]);
    wait_until("the sent count", || stats.datagrams() == 1);
}

#[test]
fn frames_closer_than_100_ms_are_rate_limited() {
    let n1mm = FakeN1mm::bind();
    let (sender, _handle, stats) = spawn_to(n1mm.addr(), "manta a");
    // Four frames fill the queue exactly (QUEUE_FRAMES), so however the
    // sender thread is scheduled, none is lost to a full queue (MAN-504).
    for _ in 0..4 {
        sender.offer(frame(14_000_000.0, 4));
    }
    n1mm.recv();
    // Both counters only grow and the burst holds four frames, so the sum
    // reaching four proves both reads are final, in any atomic ordering:
    // one send plus three drops when tight, more sends and fewer drops if
    // a loaded runner stretches one dequeue past 100 ms.
    wait_until("the burst accounted", || {
        stats.datagrams() + stats.dropped(SpectrumDrop::RateLimit) == 4
    });
    assert!(
        stats.dropped(SpectrumDrop::RateLimit) >= 2,
        "burst accounted with {:?}",
        stats
    );
    assert_eq!(stats.dropped(SpectrumDrop::QueueFull), 0);
    // Real time, not paused time (MAN-478).
    std::thread::sleep(Duration::from_millis(150));
    // A stretched burst may have sent a second datagram the recv above did
    // not consume, so it still sits in the socket buffer. Give this frame
    // contents no burst frame has and receive until it arrives: only its
    // receipt proves transmission resumed at least MIN_INTERVAL after the
    // burst, rather than a stale burst datagram (PR 515 review).
    sender.offer(frame(14_000_050.0, 4));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = n1mm.recv();
        if got.low_khz == 14_000.05 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the post-interval frame within 5 s; last received {got:?}"
        );
    }
}

#[test]
fn sending_before_n1mm_listens_is_not_an_error() {
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (sender, _handle, stats) = spawn_to(dest, "manta late");
    sender.offer(frame(7_000_000.0, 2));
    wait_until("the first send", || stats.datagrams() == 1);
    assert_eq!(stats.dropped(SpectrumDrop::SendError), 0);
    let n1mm = FakeN1mm::bind_on(dest);
    std::thread::sleep(Duration::from_millis(150));
    sender.offer(frame(7_000_000.0, 2));
    assert_eq!(n1mm.recv().name, "manta late");
    assert_eq!(stats.dropped(SpectrumDrop::SendError), 0);
}

#[test]
fn a_send_error_is_counted_and_the_feed_keeps_going() {
    // An IPv4 socket cannot send to an IPv6 destination.
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stats = Arc::new(SpectrumStats::default());
    let errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let warned = errors.clone();
    let (sender, _handle) = spectrum::spawn(
        socket,
        "[::1]:9".parse().unwrap(),
        "test",
        "manta err".into(),
        stats.clone(),
        Box::new(move |_| {
            warned.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }),
    )
    .unwrap();
    sender.offer(frame(14_000_000.0, 2));
    wait_until("one send error", || {
        stats.dropped(SpectrumDrop::SendError) == 1
    });
    std::thread::sleep(Duration::from_millis(150));
    sender.offer(frame(14_000_000.0, 2));
    wait_until("a second send error", || {
        stats.dropped(SpectrumDrop::SendError) == 2
    });
    assert_eq!(stats.datagrams(), 0);
    // The warning callback runs once, on the first failure only.
    assert_eq!(errors.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn the_sender_thread_ends_when_its_handle_drops() {
    let n1mm = FakeN1mm::bind();
    let (sender, handle, _stats) = spawn_to(n1mm.addr(), "manta end");
    drop(sender);
    handle.join().unwrap();
}

#[cfg(unix)]
mod run {
    use super::common::{expect_line, line_channel, KillOnDrop};
    use super::FakeN1mm;
    use std::collections::BTreeSet;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    /// Each of the with-server waits' budgets: one for the banner, then a
    /// second for the datagrams and their counts (a 5 s receive may overrun
    /// it once).
    const TIMEOUT: Duration = Duration::from_secs(60);

    /// A 5 s V1 IQ WAV at 48 kS/s, centred on 14 MHz, sidecar included.
    fn v1_48k(dir: &Path) -> PathBuf {
        let spec = manta_testkit::vectors::VectorSpec {
            fs: 48_000.0,
            duration_s: 5.0,
            ..manta_testkit::vectors::v1()
        };
        manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
        dir.join(format!("{}.wav", spec.name))
    }

    /// `manta run --config <config>` with no inherited MANTA_* variables.
    fn manta_command(dir: &Path, config: &str) -> Command {
        let path = dir.join("manta.toml");
        std::fs::write(&path, config).unwrap();
        let mut cmd = super::common::manta();
        cmd.args(["run", "--config"]).arg(&path);
        cmd
    }

    fn manta_run(dir: &Path, config: &str) -> Output {
        manta_command(dir, config).output().unwrap()
    }

    /// One `GET /metrics`: the whole response, or `None` while the
    /// endpoint does not answer.
    fn scrape(addr: SocketAddr) -> Option<String> {
        let mut stream = TcpStream::connect(addr).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .ok()?;
        let mut body = String::new();
        stream.read_to_string(&mut body).ok()?;
        Some(body)
    }

    fn drain(n1mm: &FakeN1mm) -> Vec<super::Received> {
        n1mm.socket
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        std::iter::from_fn(|| n1mm.try_recv()).collect()
    }

    #[test]
    fn manta_run_feeds_n1mm_from_a_file_replay_without_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let n1mm = FakeN1mm::bind();
        let config = format!(
            "[input]\ntype = \"file\"\npath = {:?}\niq = true\nrealtime = true\n\
             spectrum_udp = \"{}\"\n",
            wav.to_str().unwrap(),
            n1mm.addr()
        );
        let out = manta_run(dir.path(), &config);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "spectrum: input \"file\" sends 512 levels, 13976 to 14023.906 kHz, to {} as \
                 \"manta file\" for N1MM+'s Spectrum Display, 3 per second",
                n1mm.addr()
            )),
            "{stderr}"
        );
        let got = drain(&n1mm);
        assert!(got.len() >= 3, "{} datagrams; {stderr}", got.len());
        let mut sum = vec![0u64; 512];
        for d in &got {
            assert_eq!(d.name, "manta file");
            assert_eq!(d.low_khz, 13_976.0);
            assert_eq!(d.high_khz, 14_023.906);
            assert_eq!(d.values.len(), 512);
            for (s, v) in sum.iter_mut().zip(&d.values) {
                *s += u64::from(*v);
            }
        }
        // W1AW at +12.34 kHz: the strongest bin over the run is within one
        // 93.75 Hz bin of 14012.34 kHz.
        let peak = (0..512).max_by_key(|&i| sum[i]).unwrap();
        let peak_khz = 13_976.0 + peak as f64 * 0.09375;
        assert!(
            (peak_khz - 14_012.34).abs() <= 0.094,
            "peak at {peak_khz} kHz"
        );
    }

    /// The `[server]` table of the with-server runs: every port 0, and
    /// `metrics_max_requests_per_ip = 0` so polling `/metrics` never gets a
    /// 429 (MAN-64).
    const SERVER: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
         metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n\
         metrics_max_requests_per_ip = 0\nstatus_interval_secs = 0\n\n";

    /// `manta run` with [`SERVER`] and `inputs`, whose replays loop, so the
    /// daemon runs until this returns and kills it. Checks that `n1mm`
    /// receives the replay's span from every input in `names`, and that
    /// each input's counters are on that server's `/metrics` (D11), its
    /// datagram count at 1 or more.
    fn feeds_with_a_server(dir: &Path, n1mm: &FakeN1mm, inputs: &str, names: &[&str]) {
        let mut daemon = KillOnDrop(
            manta_command(dir, &format!("{SERVER}{inputs}"))
                .env("RUST_LOG", "info")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = line_channel(daemon.0.stderr.take().unwrap());
        let banner = expect_line(&stderr, &["listening:", "metrics="], TIMEOUT);
        let metrics: SocketAddr = banner
            .split("metrics=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|addr| addr.parse().ok())
            .unwrap_or_else(|| panic!("no metrics address in {banner:?}"));

        let deadline = Instant::now() + TIMEOUT;
        let expected: BTreeSet<String> = names.iter().map(|n| format!("manta {n}")).collect();
        let mut unseen = expected.clone();
        while !unseen.is_empty() {
            assert!(
                Instant::now() < deadline,
                "no datagram from {unseen:?} within {TIMEOUT:?}"
            );
            if let Some(d) = n1mm.try_recv() {
                assert!(expected.contains(&d.name), "unexpected source {:?}", d.name);
                assert_eq!(d.low_khz, 13_976.0);
                assert_eq!(d.high_khz, 14_023.906);
                assert_eq!(d.values.len(), 512);
                unseen.remove(&d.name);
            }
        }

        // Each counter covers at least the datagram just received.
        for name in names {
            let sent = format!("manta_spectrum_datagrams_total{{input=\"{name}\"}} ");
            let body = loop {
                let body = scrape(metrics).unwrap_or_default();
                let count = body
                    .lines()
                    .find_map(|l| l.strip_prefix(sent.as_str()))
                    .and_then(|v| v.trim().parse::<u64>().ok());
                if count.is_some_and(|n| n >= 1) {
                    break body;
                }
                assert!(
                    Instant::now() < deadline,
                    "no {sent}of 1 or more on /metrics within {TIMEOUT:?}: {body}"
                );
                std::thread::sleep(Duration::from_millis(100));
            };
            for reason in ["queue_full", "rate_limit", "send_error"] {
                let series = format!(
                    "manta_spectrum_dropped_total{{input=\"{name}\",reason=\"{reason}\"}} "
                );
                assert!(
                    body.lines().any(|l| l.starts_with(&series)),
                    "no {series}series in {body}"
                );
            }
        }
    }

    #[test]
    fn manta_run_feeds_n1mm_with_a_server_and_counts_on_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let n1mm = FakeN1mm::bind();
        let input = format!(
            "[input]\ntype = \"file\"\npath = {:?}\niq = true\nloop = true\n\
             spectrum_udp = \"{}\"\n",
            wav.to_str().unwrap(),
            n1mm.addr()
        );
        feeds_with_a_server(dir.path(), &n1mm, &input, &["file"]);
    }

    #[test]
    fn two_inputs_feed_n1mm_with_a_server_and_count_on_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let n1mm = FakeN1mm::bind();
        let mut inputs = String::new();
        for name in ["a", "b"] {
            inputs.push_str(&format!(
                "[[input]]\nname = \"{name}\"\ntype = \"file\"\npath = {:?}\niq = true\n\
                 loop = true\nspectrum_udp = \"{}\"\n\n",
                wav.to_str().unwrap(),
                n1mm.addr()
            ));
        }
        feeds_with_a_server(dir.path(), &n1mm, &inputs, &["a", "b"]);
    }

    #[test]
    fn two_inputs_send_two_sources() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let n1mm = FakeN1mm::bind();
        let mut config = String::new();
        for name in ["a", "b"] {
            config.push_str(&format!(
                "[[input]]\nname = \"{name}\"\ntype = \"file\"\npath = {:?}\niq = true\n\
                 spectrum_udp = \"{}\"\n\n",
                wav.to_str().unwrap(),
                n1mm.addr()
            ));
        }
        let out = manta_run(dir.path(), &config);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        let names: std::collections::BTreeSet<String> =
            drain(&n1mm).into_iter().map(|d| d.name).collect();
        assert!(names.contains("manta a"), "{names:?}; {stderr}");
        assert!(names.contains("manta b"), "{names:?}; {stderr}");
    }

    #[test]
    fn a_feed_on_an_input_without_an_rf_frequency_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let out = manta_run(
            dir.path(),
            "[input]\ntype = \"audio\"\nspectrum_udp = \"127.0.0.1:13064\"\n",
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains(
                "input.spectrum_udp needs a real RF frequency: this receiver reports none, so \
                 N1MM+ would draw audio offsets; set input.center_freq_hz to the radio's dial \
                 frequency"
            ),
            "{stderr}"
        );
    }
}
