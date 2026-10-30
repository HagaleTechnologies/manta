//! MAN-320 Phase 4: the WSJT-X relay worker (`relay::wsjtx::run`) against
//! real UDP datagrams on `127.0.0.1:0`. The test plays WSJT-X: it encodes
//! `NetworkMessage` datagrams (the crate's own encoder is `cfg(test)`, so a
//! small one lives here) and sends them from a second socket. The worker's
//! clock is an `AtomicI64` the test advances (D9).

use manta_server::bus::SpotBus;
use manta_server::config::{RbnUplinkConfig, UplinkSpotTypes};
use manta_server::metrics::{DropReason, Metrics, RelayKind, RelayStats};
use manta_server::rbn;
use manta_server::rbn_compare::Clock;
use manta_server::relay::wsjtx::{self, WsjtxRelay, MAX_INSTANCES};
use manta_server::relay::{RelayMode, RelaySpot};
use manta_spot::SpotType;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

const SPOTTER: &str = "W3XYZ";
/// 2023-11-14 22:13:20 UTC.
const T0: i64 = 1_700_000_000;
const WAIT: Duration = Duration::from_secs(5);

/// A QDataStream big-endian writer for WSJT-X datagrams.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn header(kind: u32) -> Self {
        let mut w = Writer::default();
        w.u32(0xadbc_cbda).u32(3).u32(kind);
        w
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.0.push(u8::from(v));
        self
    }
    fn utf8(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
}

fn heartbeat(id: &str) -> Vec<u8> {
    let mut w = Writer::header(0);
    w.utf8(id).u32(3).utf8("2.6.1").utf8("");
    w.0
}

fn status(id: &str, dial_hz: u64, mode: &str) -> Vec<u8> {
    let mut w = Writer::header(1);
    w.utf8(id)
        .u64(dial_hz)
        .utf8(mode)
        .utf8("")
        .utf8("")
        .utf8(mode)
        .bool(false)
        .bool(false)
        .bool(true);
    w.0
}

#[derive(Clone, Copy)]
struct Flags {
    new: bool,
    low_confidence: bool,
    off_air: bool,
}

const NEW: Flags = Flags {
    new: true,
    low_confidence: false,
    off_air: false,
};

fn decode_with(id: &str, flags: Flags, snr: i32, df: u32, message: &str) -> Vec<u8> {
    let mut w = Writer::header(2);
    w.utf8(id)
        .bool(flags.new)
        .u32(80_000_000)
        .i32(snr)
        .f64(0.1)
        .u32(df)
        .utf8("~")
        .utf8(message)
        .bool(flags.low_confidence)
        .bool(flags.off_air);
    w.0
}

fn decode(id: &str, snr: i32, df: u32, message: &str) -> Vec<u8> {
    decode_with(id, NEW, snr, df, message)
}

struct Harness {
    tx: UdpSocket,
    to: SocketAddr,
    now: Arc<AtomicI64>,
    stats: Arc<RelayStats>,
    bus: Arc<SpotBus>,
    relayed: broadcast::Receiver<RelaySpot>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Harness {
    async fn send(&self, datagram: &[u8]) {
        self.tx.send_to(datagram, self.to).await.unwrap();
    }

    async fn next_spot(&mut self) -> RelaySpot {
        tokio::time::timeout(WAIT, self.relayed.recv())
            .await
            .expect("timed out waiting for a relayed spot")
            .unwrap()
    }

    /// Waits until `reason` has been counted `n` times.
    async fn dropped(&self, reason: DropReason, n: u64) {
        let stats = self.stats.clone();
        tokio::time::timeout(WAIT, async move {
            while stats.dropped_total(reason) < n {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {} x{n}", reason.label()));
    }

    /// Nothing more was relayed (call after a drop has been counted).
    fn nothing_relayed(&mut self) {
        assert!(
            matches!(
                self.relayed.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a spot was relayed"
        );
    }
}

async fn spawn_relay() -> Harness {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let to = socket.local_addr().unwrap();
    let metrics = Metrics::new();
    let stats = metrics.register_relay(to.to_string(), RelayKind::Wsjtx);
    let bus = Arc::new(SpotBus::new(
        96_000.0,
        SystemTime::UNIX_EPOCH + Duration::from_secs(T0 as u64),
        0,
    ));
    let relayed = bus.subscribe_relay();
    let now = Arc::new(AtomicI64::new(T0));
    let clock_now = now.clone();
    let clock: Clock = Arc::new(move || clock_now.load(Ordering::SeqCst));
    let (shutdown, shutdown_rx) = watch::channel(false);
    let cfg = WsjtxRelay {
        label: to.to_string(),
        callsign: SPOTTER.to_string(),
    };
    let task = tokio::spawn(wsjtx::run(
        cfg,
        socket,
        clock,
        bus.clone(),
        stats.clone(),
        shutdown_rx,
    ));
    let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    Harness {
        tx,
        to,
        now,
        stats,
        bus,
        relayed,
        shutdown,
        task,
    }
}

#[tokio::test]
async fn a_new_ft8_cq_after_a_status_is_relayed() {
    let mut h = spawn_relay().await;
    h.send(&status("WSJT-X", 14_074_000, "FT8")).await;
    h.send(&decode("WSJT-X", -12, 1520, "CQ K1ABC FN42")).await;
    let spot = h.next_spot().await;
    assert_eq!(
        spot,
        RelaySpot {
            spotter: Arc::from(SPOTTER),
            callsign: "K1ABC".to_string(),
            freq_hz: 14_075_520.0,
            mode: RelayMode::Ft8,
            snr_db: -12,
            spot_type: SpotType::Cq,
            unix_ts: T0,
        }
    );
    assert_eq!(h.stats.spots_total(), 1);
    assert!(h.stats.up());
    assert_eq!(
        rbn::format_relay_line(&spot),
        "DX de W3XYZ-#:  14075.52  K1ABC          FT8   -12 dB         CQ      2213Z"
    );
    let _ = h.shutdown.send(true);
}

#[tokio::test]
async fn ft4_is_relayed_as_ft4() {
    let mut h = spawn_relay().await;
    h.send(&status("WSJT-X", 14_080_000, "FT4")).await;
    h.send(&decode("WSJT-X", 3, 1000, "CQ POTA W9XYZ EN37"))
        .await;
    let spot = h.next_spot().await;
    assert_eq!(spot.mode, RelayMode::Ft4);
    assert_eq!(spot.callsign, "W9XYZ");
    assert_eq!(spot.freq_hz, 14_081_000.0);
    assert_eq!(spot.snr_db, 3);
}

#[tokio::test]
async fn a_replay_off_air_or_low_confidence_decode_is_dropped() {
    let mut h = spawn_relay().await;
    h.send(&status("WSJT-X", 14_074_000, "FT8")).await;
    let replay = Flags { new: false, ..NEW };
    let off_air = Flags {
        off_air: true,
        ..NEW
    };
    let low = Flags {
        low_confidence: true,
        ..NEW
    };
    h.send(&decode_with("WSJT-X", replay, -5, 500, "CQ K1ABC FN42"))
        .await;
    h.send(&decode_with("WSJT-X", off_air, -5, 500, "CQ K1ABD FN42"))
        .await;
    h.send(&decode_with("WSJT-X", low, -5, 500, "CQ K1ABE FN42"))
        .await;
    h.dropped(DropReason::Replay, 1).await;
    h.dropped(DropReason::OffAir, 1).await;
    h.dropped(DropReason::LowConfidence, 1).await;
    h.nothing_relayed();
    assert_eq!(h.stats.spots_total(), 0);
}

#[tokio::test]
async fn a_decode_before_any_status_is_dropped_as_no_dial() {
    let mut h = spawn_relay().await;
    h.send(&decode("WSJT-X", -5, 500, "CQ K1ABC FN42")).await;
    h.dropped(DropReason::NoDial, 1).await;
    // A Status with dial 0 is no dial either.
    h.send(&status("WSJT-X", 0, "FT8")).await;
    h.send(&decode("WSJT-X", -5, 500, "CQ K1ABC FN42")).await;
    h.dropped(DropReason::NoDial, 2).await;
    h.nothing_relayed();
}

#[tokio::test]
async fn a_jt65_instance_is_dropped_as_mode() {
    let mut h = spawn_relay().await;
    h.send(&status("WSJT-X", 14_076_000, "JT65")).await;
    h.send(&decode("WSJT-X", -5, 500, "CQ K1ABC FN42")).await;
    h.dropped(DropReason::Mode, 1).await;
    h.nothing_relayed();
}

#[tokio::test]
async fn non_cq_and_hashed_callers_are_dropped() {
    let mut h = spawn_relay().await;
    h.send(&status("WSJT-X", 14_074_000, "FT8")).await;
    for msg in ["K1ABC W9XYZ EN37", "W9XYZ K1ABC RR73", "TNX 73 GL"] {
        h.send(&decode("WSJT-X", -5, 500, msg)).await;
    }
    for msg in ["CQ <...> FN42", "CQ"] {
        h.send(&decode("WSJT-X", -5, 500, msg)).await;
    }
    h.dropped(DropReason::NotCq, 3).await;
    h.dropped(DropReason::NoCall, 2).await;
    h.nothing_relayed();
}

#[tokio::test]
async fn the_same_call_mode_and_band_within_600_s_is_a_duplicate() {
    let mut h = spawn_relay().await;
    h.send(&status("A", 14_074_000, "FT8")).await;
    h.send(&decode("A", -5, 500, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.unix_ts, T0);

    h.now.store(T0 + 599, Ordering::SeqCst);
    h.send(&decode("A", -5, 900, "CQ K1ABC FN42")).await;
    h.dropped(DropReason::Duplicate, 1).await;
    h.nothing_relayed();

    h.now.store(T0 + 601, Ordering::SeqCst);
    h.send(&decode("A", -5, 500, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.unix_ts, T0 + 601);

    // The same call on 40 m is another band: relayed at once.
    h.send(&status("B", 7_074_000, "FT8")).await;
    h.send(&decode("B", -5, 500, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.freq_hz, 7_074_500.0);
    assert_eq!(h.stats.dropped_total(DropReason::Duplicate), 1);
}

#[tokio::test]
async fn two_instances_keep_their_own_dial_frequencies() {
    let mut h = spawn_relay().await;
    h.send(&status("A", 14_074_000, "FT8")).await;
    h.send(&status("B", 7_074_000, "FT8")).await;
    h.send(&decode("A", -5, 1000, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.freq_hz, 14_075_000.0);
    h.send(&decode("B", -5, 1000, "CQ W9XYZ EN37")).await;
    assert_eq!(h.next_spot().await.freq_hz, 7_075_000.0);
}

#[tokio::test]
async fn a_malformed_datagram_is_counted_and_the_listener_keeps_going() {
    let mut h = spawn_relay().await;
    h.send(b"not a wsjt-x datagram").await;
    let mut truncated = decode("A", -5, 500, "CQ K1ABC FN42");
    truncated.truncate(20);
    h.send(&truncated).await;
    h.dropped(DropReason::BadDatagram, 2).await;
    assert!(
        !h.stats.up(),
        "a malformed datagram is not a WSJT-X sign of life"
    );

    h.send(&status("A", 14_074_000, "FT8")).await;
    h.send(&decode("A", -5, 500, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.callsign, "K1ABC");
}

/// MAN-478: real UDP under `start_paused` raced on macOS. The clock could
/// auto-advance past the 60 s window before kqueue reported the second
/// heartbeat, which then counted as heard 10 s late and kept the relay up
/// at the "61 s of silence" check. Each datagram is now sent and seen in
/// real time; time is paused only for the silent stretches, when no
/// assertion waits on socket I/O.
#[tokio::test]
async fn up_follows_datagrams_within_60_s() {
    let h = spawn_relay().await;
    assert!(!h.stats.up());
    h.send(&heartbeat("WSJT-X")).await;
    let stats = h.stats.clone();
    tokio::time::timeout(WAIT, async {
        while !stats.up() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("a heartbeat sets up");

    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(50)).await;
    assert!(h.stats.up());
    tokio::time::resume();
    // The relay's 60 s deadline is now 10 s of real time away. A malformed
    // datagram (counted, but not a sign of life) sent after the heartbeat
    // on the same loopback socket fences it: once it is counted, the
    // heartbeat has been heard.
    h.send(&heartbeat("WSJT-X")).await;
    h.send(b"not a wsjt-x datagram").await;
    h.dropped(DropReason::BadDatagram, 1).await;
    assert!(h.stats.up());
    tokio::time::pause();
    // 70 s after the first heartbeat, 20 s after the second: still up.
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(h.stats.up());

    tokio::time::sleep(Duration::from_secs(41)).await;
    assert!(!h.stats.up(), "61 s of silence takes the relay down");
}

#[tokio::test]
async fn shutdown_stops_the_listener() {
    let h = spawn_relay().await;
    h.send(&heartbeat("WSJT-X")).await;
    let _ = h.shutdown.send(true);
    tokio::time::timeout(WAIT, h.task)
        .await
        .expect("the listener did not stop")
        .unwrap();
}

#[tokio::test]
async fn the_instance_map_is_bounded() {
    let mut h = spawn_relay().await;
    for i in 0..=MAX_INSTANCES {
        h.send(&status(&format!("I{i}"), 14_074_000, "FT8")).await;
    }
    // I0 was the oldest-heard of 65: evicted, so its decode has no dial.
    h.send(&decode("I0", -5, 500, "CQ K1ABC FN42")).await;
    h.dropped(DropReason::NoDial, 1).await;
    h.nothing_relayed();
    // I1 is still known.
    h.send(&decode("I1", -5, 500, "CQ K1ABC FN42")).await;
    assert_eq!(h.next_spot().await.callsign, "K1ABC");
}

/// MAN-320 Scenario 2, end to end inside manta-server: UDP ->
/// `relay::wsjtx::run` -> `uplink::serve` (`dry_run = false`) -> a mock
/// collector.
///
/// ```gherkin
/// Scenario: A WSJT-X decode is relayed as a digital spot
///   Given WSJT-X is sending decodes by UDP to manta
///   When a decode arrives
///   Then it is forwarded as a digital-mode spot
/// ```
#[tokio::test]
async fn a_wsjtx_decode_reaches_the_collector_as_a_digital_spot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = spawn_relay().await;
    let metrics = Metrics::new();
    let target = metrics.register_uplink_target(format!("127.0.0.1:{port}"), true);
    let cfg = RbnUplinkConfig {
        enabled: true,
        target_host: "127.0.0.1".to_string(),
        target_port: port,
        login_callsign: None,
        dry_run: false,
        spot_types: UplinkSpotTypes::default(),
        max_spots_per_minute: 600,
    };
    let (_uplink_shutdown, uplink_rx) = watch::channel(false);
    tokio::spawn(manta_server::uplink::serve(
        cfg,
        SPOTTER.to_string(),
        h.bus.clone(),
        target,
        uplink_rx,
    ));

    let (socket, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the uplink did not connect")
        .unwrap();
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);
    wr.write_all(b"login: \r\n").await.unwrap();
    let mut login = String::new();
    tokio::time::timeout(WAIT, reader.read_line(&mut login))
        .await
        .expect("no login line")
        .unwrap();
    assert_eq!(login.trim_end(), SPOTTER);

    h.send(&status("WSJT-X", 14_074_000, "FT8")).await;
    h.send(&decode("WSJT-X", -12, 1520, "CQ K1ABC FN42")).await;
    let mut line = String::new();
    tokio::time::timeout(WAIT, reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the relayed spot line")
        .unwrap();
    assert_eq!(
        line.trim_end(),
        "DX de W3XYZ-#:  14075.52  K1ABC          FT8   -12 dB         CQ      2213Z"
    );
    assert_eq!(h.stats.spots_total(), 1);
}
