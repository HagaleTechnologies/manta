//! MAN-32 acceptance scenarios:
//!   Scenario: manta pushes its own spots to the RBN's collection endpoint
//!     Given manta is configured with RBN node credentials/target
//!     When manta validates and emits a spot
//!     Then the spot is forwarded to the RBN's own spot-collection endpoint
//!     And the forwarded spot uses the format RBN's ingestion expects
//!
//!   Scenario: A dry-run configuration suppresses the actual RBN send
//!     Given manta is configured with RBN node credentials/target
//!     And the outbound connection is set to dry-run mode
//!     When manta validates and emits a spot
//!     Then the spot is NOT forwarded to the RBN's collection endpoint
//!     And the spot is still visible through manta's local telnet/JSON output
//!
//! The mock listener in this file stands in for RBN's own collection
//! server -- manta is the connecting *client* here, the reverse of
//! `telnet_acceptance.rs`'s role.

use manta_server::bus::SpotBus;
use manta_server::config::RbnUplinkConfig;
use manta_server::metrics::Metrics;
use manta_server::rbn;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ";

fn sample_spot() -> Spot {
    Spot {
        callsign: "JA1ABC".to_string(),
        freq_hz: 14_027_100.0,
        snr_db: 23.0,
        wpm: 28.0,
        spot_type: SpotType::Cq,
        confidence: 0.9,
        track_id: 1,
        sample_ts: 0,
    }
}

fn uplink_config(target_port: u16, dry_run: bool) -> RbnUplinkConfig {
    RbnUplinkConfig {
        enabled: true,
        target_host: "127.0.0.1".to_string(),
        target_port,
        login_callsign: None,
        dry_run,
    }
}

struct Harness {
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

/// Spawns `uplink::serve` against `target_port` (a mock RBN listener the
/// test itself controls) and returns the shared bus/metrics/shutdown
/// handles the test drives.
fn spawn_uplink(target_port: u16, dry_run: bool) -> Harness {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let cfg = uplink_config(target_port, dry_run);
    let bus2 = bus.clone();
    let metrics2 = metrics.clone();
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus2, metrics2, shutdown_rx)
            .await;
    });

    Harness {
        bus,
        metrics,
        shutdown_tx,
    }
}

/// Accepts one connection on `listener`, performs the login side of the
/// handshake (send a login prompt, read back the client's login line),
/// and returns the login line plus the still-open reader/writer for the
/// test to keep asserting on.
async fn mock_rbn_accept_and_login(
    listener: &TcpListener,
) -> (
    String,
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let (socket, _peer) = listener.accept().await.unwrap();
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    wr.write_all(b"login: \r\n").await.unwrap();
    let mut login_line = String::new();
    reader.read_line(&mut login_line).await.unwrap();

    (login_line, reader, wr)
}

#[tokio::test]
async fn logs_in_and_forwards_a_published_spot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(login_line.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(&spot, STATION_CALL, harness.bus.unix_ts_for(spot.sample_ts));
    harness.bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the forwarded spot line")
        .unwrap();

    assert_eq!(line.trim_end(), expected);
    assert_eq!(harness.metrics.uplink_sent_total(), 1);
    assert!(harness.metrics.uplink_connected());

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn dry_run_logs_in_but_does_not_forward_the_spot_line() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), true);

    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(
        login_line.trim_end(),
        STATION_CALL,
        "dry-run must still complete the login handshake"
    );

    harness.bus.publish(sample_spot());

    let mut line = String::new();
    let result = tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line))
        .await;
    assert!(
        result.is_err(),
        "dry-run must not transmit the spot line, got: {line:?}"
    );
    assert_eq!(harness.metrics.uplink_sent_total(), 0);
    assert_eq!(harness.metrics.uplink_suppressed_total(), 1);

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn dry_run_does_not_affect_other_bus_subscribers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), true);
    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    // A second, independent subscriber standing in for the telnet/JSON
    // servers -- dry-run must be local to the uplink task, never a
    // bus-wide suppression.
    let mut other_rx = harness.bus.subscribe();
    harness.bus.publish(sample_spot());

    let received = tokio::time::timeout(Duration::from_secs(5), other_rx.recv())
        .await
        .expect("other subscriber timed out")
        .unwrap();
    assert_eq!(received.spot.callsign, "JA1ABC");

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn reconnects_after_the_remote_end_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    // First connection: accept, complete login, then drop it immediately
    // (simulating an RBN-side disconnect).
    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    // `_reader`/`_wr` drop here, closing the socket.
    drop(_reader);
    drop(_wr);

    // The uplink must come back and log in again.
    let (login_line2, _reader2, _wr2) =
        tokio::time::timeout(Duration::from_secs(5), mock_rbn_accept_and_login(&listener))
            .await
            .expect("uplink did not reconnect in time");
    assert_eq!(login_line2.trim_end(), STATION_CALL);
    assert!(harness.metrics.uplink_reconnects_total() >= 1);

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn shutdown_signal_stops_the_reconnect_loop_without_a_new_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    drop(_reader);
    drop(_wr);

    // Signal shutdown immediately -- before asserting on a reconnect --
    // so the reconnect loop should observe it during its backoff sleep
    // and stop, rather than accepting a second connection.
    let _ = harness.shutdown_tx.send(true);

    let second_attempt = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(
        second_attempt.is_err(),
        "uplink attempted to reconnect after shutdown was signaled"
    );
}

#[tokio::test]
async fn disabled_uplink_makes_no_connection_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch));
    let metrics = Arc::new(Metrics::new());
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let mut cfg = uplink_config(addr.port(), false);
    cfg.enabled = false;
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus, metrics, shutdown_rx)
            .await;
    });

    let attempt = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(
        attempt.is_err(),
        "a disabled uplink must never attempt a connection"
    );
}
