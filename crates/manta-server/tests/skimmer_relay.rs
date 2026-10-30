//! MAN-320 Phase 3: the skimmer relay worker against a mock skimmer
//! (`TcpListener` on `127.0.0.1:0`) that sends CW Skimmer's greeting with
//! the callsign prompt unterminated, reads the login line, then writes the
//! lines each test chooses. Every wait is bounded.

use manta_server::bus::SpotBus;
use manta_server::config::{RbnUplinkConfig, UplinkSpotTypes};
use manta_server::metrics::{DropReason, Metrics, RelayKind, RelayStats};
use manta_server::rbn_compare::Clock;
use manta_server::relay::skimmer::{self, SkimmerRelay};
use manta_server::relay::{RelayMode, RelaySpot};
use manta_spot::SpotType;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

const RELAY_CALL: &str = "W3XYZ-2";
const STATION_CALL: &str = "W3XYZ";
/// 03:12Z, so a relayed line's time column reads `0312Z`.
const NOW: i64 = 3 * 3600 + 12 * 60;
const WAIT: Duration = Duration::from_secs(5);

struct Harness {
    stats: Arc<RelayStats>,
    relay_rx: broadcast::Receiver<RelaySpot>,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

fn spawn_relay(port: u16) -> Harness {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(96_000.0, epoch, 0));
    spawn_relay_on(port, bus)
}

fn spawn_relay_on(port: u16, bus: Arc<SpotBus>) -> Harness {
    spawn_relay_with(port, bus, skimmer::KEEPALIVE_INTERVAL)
}

fn spawn_relay_with(port: u16, bus: Arc<SpotBus>, keepalive_every: Duration) -> Harness {
    let metrics = Metrics::new();
    let stats = metrics.register_relay(format!("127.0.0.1:{port}"), RelayKind::Skimmer);
    let relay_rx = bus.subscribe_relay();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock: Clock = Arc::new(|| NOW);
    let cfg = SkimmerRelay {
        host: "127.0.0.1".to_string(),
        port,
        callsign: RELAY_CALL.to_string(),
    };
    let task = tokio::spawn(skimmer::run_with_keepalive(
        cfg,
        keepalive_every,
        clock,
        bus,
        stats.clone(),
        shutdown_rx,
    ));
    Harness {
        stats,
        relay_rx,
        shutdown_tx,
        task,
    }
}

/// Accepts the worker, sends CW Skimmer's greeting with an unterminated
/// prompt, and returns the login line plus the open halves.
async fn accept_and_login(
    listener: &TcpListener,
) -> (String, BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let (socket, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the relay never connected")
        .unwrap();
    let (rd, mut wr) = socket.into_split();
    wr.write_all(b"CW Skimmer Server\r\n\r\nPlease enter your callsign: ")
        .await
        .unwrap();
    let mut reader = BufReader::new(rd);
    let mut login = String::new();
    tokio::time::timeout(WAIT, reader.read_line(&mut login))
        .await
        .expect("no login line")
        .unwrap();
    (login.trim_end().to_string(), reader, wr)
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn next_relay(rx: &mut broadcast::Receiver<RelaySpot>) -> RelaySpot {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("no relayed spot")
        .unwrap()
}

#[tokio::test]
async fn logs_in_with_the_relay_callsign() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (login, _r, _w) = accept_and_login(&listener).await;
    assert_eq!(login, RELAY_CALL);
    wait_for("stats.up()", || h.stats.up()).await;
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn forwards_its_own_cw_spots_in_either_layout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    w.write_all(
        b"DX de W3XYZ-2-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      0312Z\r\n\
          DX de W3XYZ-2-#:  7023.4  K1ABC          12 dB  31 WPM  DE      0312Z\r\n",
    )
    .await
    .unwrap();
    let a = next_relay(&mut h.relay_rx).await;
    assert_eq!(
        a,
        RelaySpot {
            spotter: Arc::from(RELAY_CALL),
            callsign: "JA1ABC".to_string(),
            freq_hz: 14_027_100.0,
            mode: RelayMode::Cw { wpm: 28 },
            snr_db: 30,
            spot_type: SpotType::Cq,
            unix_ts: NOW,
        }
    );
    let b = next_relay(&mut h.relay_rx).await;
    assert_eq!(b.callsign, "K1ABC");
    assert!((b.freq_hz - 7_023_400.0).abs() < 1e-6);
    assert_eq!(b.mode, RelayMode::Cw { wpm: 31 });
    assert_eq!(b.snr_db, 12);
    assert_eq!(b.spot_type, SpotType::De);
    assert_eq!(h.stats.spots_total(), 2);
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn spots_made_under_another_call_are_dropped_and_counted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    w.write_all(
        b"DX de W3XYZ-#:  14027.10  JA1ABC  CW  30 dB  28 WPM  CQ  0312Z\r\n\
          DX de K1TTT-#:  14027.10  JA1ABC  CW  30 dB  28 WPM  CQ  0312Z\r\n",
    )
    .await
    .unwrap();
    wait_for("two other-spotter drops", || {
        h.stats.dropped_total(DropReason::OtherSpotter) == 2
    })
    .await;
    assert!(h.relay_rx.try_recv().is_err(), "nothing may be published");
    assert_eq!(h.stats.spots_total(), 0);
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn rtty_lines_are_dropped_as_not_cw() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    w.write_all(b"DX de W3XYZ-2-#: 14080.00 JA1ABC RTTY 21 dB 45 BPS CQ 0312Z\r\n")
        .await
        .unwrap();
    wait_for("a not_cw drop", || {
        h.stats.dropped_total(DropReason::NotCw) == 1
    })
    .await;
    assert!(h.relay_rx.try_recv().is_err());
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn an_implausible_call_is_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    w.write_all(b"DX de W3XYZ-2-#: 14027.10 ABCDEF CW 21 dB 25 WPM CQ 0312Z\r\n")
        .await
        .unwrap();
    wait_for("a bad_call drop", || {
        h.stats.dropped_total(DropReason::BadCall) == 1
    })
    .await;
    assert!(h.relay_rx.try_recv().is_err());
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn banners_prompts_and_talk_are_ignored_uncounted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    w.write_all(
        b"Hello W3XYZ-2, this is CW Skimmer\r\nde W3XYZ-2-# >\r\nTo ALL de K1TTT: hi\r\n\
          DX de W3XYZ-2-#: 14027.10 JA1ABC CW 21 dB 25 WPM CQ 0312Z\r\n",
    )
    .await
    .unwrap();
    // The spot after them arrives; nothing before it was counted.
    assert_eq!(next_relay(&mut h.relay_rx).await.callsign, "JA1ABC");
    for reason in [
        DropReason::NotCw,
        DropReason::OtherSpotter,
        DropReason::BadCall,
    ] {
        assert_eq!(h.stats.dropped_total(reason), 0, "{reason:?}");
    }
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn reconnects_after_the_skimmer_closes_and_logs_in_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (login, r, w) = accept_and_login(&listener).await;
    assert_eq!(login, RELAY_CALL);
    wait_for("up", || h.stats.up()).await;
    drop((r, w));
    wait_for("down after the close", || !h.stats.up()).await;
    let (login2, _r2, _w2) = accept_and_login(&listener).await;
    assert_eq!(login2, RELAY_CALL);
    wait_for("up again", || h.stats.up()).await;
    let _ = h.shutdown_tx.send(true);
}

#[tokio::test]
async fn shutdown_stops_the_worker_during_login_and_while_reading() {
    // During login: the skimmer accepts but never prompts.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (_held, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let _ = h.shutdown_tx.send(true);
    tokio::time::timeout(WAIT, h.task)
        .await
        .expect("the worker ignored shutdown during login")
        .unwrap();

    // While reading.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, _w) = accept_and_login(&listener).await;
    wait_for("up", || h.stats.up()).await;
    let _ = h.shutdown_tx.send(true);
    tokio::time::timeout(WAIT, h.task)
        .await
        .expect("the worker ignored shutdown while reading")
        .unwrap();
}

#[tokio::test]
async fn an_oversized_line_ends_the_session_and_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, _r, mut w) = accept_and_login(&listener).await;
    wait_for("up", || h.stats.up()).await;
    let junk = vec![b'x'; manta_server::bounded_io::MAX_LINE_BYTES + 16];
    let _ = w.write_all(&junk).await;
    let (login2, _r2, _w2) = accept_and_login(&listener).await;
    assert_eq!(login2, RELAY_CALL);
    let _ = h.shutdown_tx.send(true);
}

/// MAN-320 Scenario 1: Given a second CW skimmer serving spots by telnet on the LAN,
/// When it is added to manta as a secondary source,
/// Then its spots are forwarded through manta's uplink under the secondary's callsign.
#[tokio::test]
async fn a_second_skimmers_spot_reaches_the_collector_under_its_own_call() {
    let collector = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let skimmer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(96_000.0, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let port = collector.local_addr().unwrap().port();
    let target = metrics.register_uplink_target(format!("127.0.0.1:{port}"), true);
    let (up_tx, up_rx) = watch::channel(false);
    let cfg = RbnUplinkConfig {
        enabled: true,
        target_host: "127.0.0.1".to_string(),
        target_port: port,
        login_callsign: None,
        dry_run: false,
        spot_types: UplinkSpotTypes::default(),
        max_spots_per_minute: 600,
    };
    let bus2 = bus.clone();
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus2, target, up_rx).await;
    });
    let (socket, _) = tokio::time::timeout(WAIT, collector.accept())
        .await
        .unwrap()
        .unwrap();
    let (crd, mut cwr) = socket.into_split();
    cwr.write_all(b"login: \r\n").await.unwrap();
    let mut creader = BufReader::new(crd);
    let mut uplink_login = String::new();
    tokio::time::timeout(WAIT, creader.read_line(&mut uplink_login))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(uplink_login.trim_end(), STATION_CALL);

    let h = spawn_relay_on(skimmer_listener.local_addr().unwrap().port(), bus);
    let (login, _r, mut w) = accept_and_login(&skimmer_listener).await;
    assert_eq!(login, RELAY_CALL);
    w.write_all(
        b"DX de W3XYZ-2-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      0312Z\r\n",
    )
    .await
    .unwrap();

    let mut line = String::new();
    tokio::time::timeout(WAIT, creader.read_line(&mut line))
        .await
        .expect("the collector never got the relayed spot")
        .unwrap();
    assert_eq!(
        line.trim_end(),
        "DX de W3XYZ-2-#: 14027.10 JA1ABC         CW    30 dB  28 WPM  CQ      0312Z"
    );
    assert_eq!(metrics.uplink_sent_total(), 1);
    let _ = h.shutdown_tx.send(true);
    let _ = up_tx.send(true);
}

/// D10: no idle timeout -- two silent hours keep the session.
///
/// MAN-478: the connect and login run in real time. Under `start_paused`
/// tokio auto-advanced past `accept_and_login`'s 5 s deadlines before
/// kqueue reported the loopback connect, so this failed on macOS. Time is
/// paused only once the session is up. From then on no assertion waits on
/// socket I/O: the keepalive writes land in an almost empty send buffer
/// (the socket is already known writable), the drain reads' timeouts are
/// ignored, and `up` is an atomic. The reconnect check runs after
/// `resume`, in real time, so a reconnect made during the paused hours
/// (already queued in the listener's backlog) is seen.
#[tokio::test]
async fn a_quiet_skimmer_is_not_disconnected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = spawn_relay(listener.local_addr().unwrap().port());
    let (_login, mut r, _w) = accept_and_login(&listener).await;
    wait_for("up", || h.stats.up()).await;
    tokio::time::pause();
    // Drain keepalives for two hours; the session never drops.
    let end = tokio::time::Instant::now() + Duration::from_secs(2 * 3600);
    while tokio::time::Instant::now() < end {
        let mut line = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(600), r.read_line(&mut line)).await;
        assert!(h.stats.up(), "a quiet skimmer was disconnected");
    }
    tokio::time::resume();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_err(),
        "the relay reconnected"
    );
    assert!(h.stats.up(), "a quiet skimmer was disconnected");
    let _ = h.shutdown_tx.send(true);
}

/// D10: production sends a keepalive every 300 s.
#[test]
fn the_keepalive_interval_is_300_s() {
    assert_eq!(skimmer::KEEPALIVE_INTERVAL, Duration::from_secs(300));
}

/// D10: a blank CRLF keepalive goes out every interval -- never early, and
/// again and again. Real time with a 250 ms interval (MAN-478: under
/// paused time the keepalive read raced the 400 s read deadline the same
/// way the connect did). `start` is taken before the relay exists, so the
/// relay's own interval can't start earlier and the lower bound is exact.
#[tokio::test]
async fn sends_a_blank_keepalive_every_interval() {
    const EVERY: Duration = Duration::from_millis(250);
    // A loaded CI runner may deliver late; it can never deliver early.
    const LATE_OK: Duration = Duration::from_secs(3);
    let start = tokio::time::Instant::now();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(96_000.0, epoch, 0));
    let h = spawn_relay_with(listener.local_addr().unwrap().port(), bus, EVERY);
    let (_login, mut r, _w) = accept_and_login(&listener).await;
    for n in 1..=3u32 {
        let mut line = String::new();
        tokio::time::timeout(EVERY * n + LATE_OK, r.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no keepalive {n}"))
            .unwrap();
        assert_eq!(line, "\r\n");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= EVERY * n && elapsed < EVERY * n + LATE_OK,
            "keepalive {n} at {elapsed:?}"
        );
    }
    let _ = h.shutdown_tx.send(true);
}
