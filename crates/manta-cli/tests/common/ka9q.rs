//! MAN-140's fake radiod and the live-`run` helpers around it, shared by
//! `ka9q_input.rs` and `callsign_lookup.rs` (MAN-318). Gated like them:
//! loopback multicast is unverified on Windows CI.

use manta_input::ka9q::wire::{self, ChannelStatus, Encoding, RtpHeader};
use manta_input::IqSource;
use num_complex::Complex32;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

pub const SSRC: u32 = 14_000_000;
pub const RATE: u64 = 96_000;
pub const PER_PACKET: usize = 180;
pub const READY_MARKER: &str = "manta: listening;";

pub fn manta() -> Command {
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
pub fn unique_group() -> Ipv4Addr {
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
pub fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}

pub fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The prototype's sender: loopback interface, TTL 0, loop on.
pub fn sender() -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
    s.set_multicast_ttl_v4(0).unwrap();
    s.set_multicast_loop_v4(true).unwrap();
    s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
        .unwrap();
    s.into()
}

/// V1 shortened to 30 s, as `tests/cli.rs`'s `short_v1`.
pub fn v1_fixture(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

pub fn wav_samples(path: &Path) -> Vec<Complex32> {
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

pub fn channel_status(group: Ipv4Addr, data_port: u16, agc: bool) -> ChannelStatus {
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
pub struct FakeRadiod {
    pub stop: Arc<AtomicBool>,
    pub threads: Vec<thread::JoinHandle<()>>,
}

impl FakeRadiod {
    pub fn status_only(group: Ipv4Addr, status: ChannelStatus) -> Self {
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
    pub fn stream(
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

pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn metric_value(body: &str, metric_with_labels: &str) -> Option<u64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find(|l| l.starts_with(prefix.as_str()))
        .and_then(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

pub fn scrape_metrics(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    Some(body)
}
