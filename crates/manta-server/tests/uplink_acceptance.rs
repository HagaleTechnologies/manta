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
//! MAN-42 acceptance scenarios:
//!   Scenario: Spots are forwarded to every configured RBN target
//!     Given manta's uplink is configured with two RBN collection targets
//!     When manta validates and emits a spot
//!     Then the spot is forwarded to both configured targets
//!
//!   Scenario: One target failing does not stop delivery to the others
//!     Given manta's uplink is configured with two RBN collection targets
//!     And one target's connection is down
//!     When manta validates and emits a spot
//!     Then the spot is still forwarded to the target that is reachable
//!     And the unreachable target's connection is retried independently
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
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
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

/// Spawns one `uplink::serve` task per config, all sharing the same
/// bus/metrics/shutdown -- mirroring `start_spot_server`'s real MAN-42
/// wiring (one independent task per configured `[[rbn_uplink]]` target).
fn spawn_uplinks(configs: Vec<RbnUplinkConfig>) -> Harness {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    for cfg in configs {
        let bus2 = bus.clone();
        let metrics2 = metrics.clone();
        let shutdown_rx2 = shutdown_rx.clone();
        tokio::spawn(async move {
            manta_server::uplink::serve(
                cfg,
                STATION_CALL.to_string(),
                bus2,
                metrics2,
                shutdown_rx2,
            )
            .await;
        });
    }

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
    let result =
        tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await;
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

/// MAN-58 finding 1: a target that accepts the connection but never sends
/// a login prompt line used to hang this task's `read_line` indefinitely
/// and ignore shutdown. The bounded, shutdown-raced replacement must
/// close the connection promptly once shutdown fires, even mid-wait.
#[tokio::test]
async fn shutdown_signal_interrupts_a_hanging_login_prompt_read() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (socket, _peer) = listener.accept().await.unwrap();
    let (rd, _wr) = socket.into_split();
    let mut reader = BufReader::new(rd);
    // Deliberately never send a login prompt.

    let _ = harness.shutdown_tx.send(true);

    let mut buf = String::new();
    let read_result = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut buf))
        .await
        .expect(
            "uplink did not close the connection promptly after shutdown while \
             still waiting on the login prompt",
        );
    assert_eq!(
        read_result.unwrap(),
        0,
        "expected EOF (client closed) after shutdown, not more data"
    );
}

/// MAN-58 finding 2: an unterminated response line from the target past
/// `bounded_io`'s length cap used to grow the discard buffer without
/// bound. It must instead be rejected, tearing down the connection (and
/// therefore triggering the normal reconnect/backoff path) rather than
/// hanging or leaking memory.
#[tokio::test]
async fn an_oversized_unterminated_response_line_forces_a_reconnect_instead_of_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, mut wr) = mock_rbn_accept_and_login(&listener).await;

    // Past bounded_io::MAX_LINE_BYTES (1024), no newline.
    wr.write_all(&vec![b'A'; 2000]).await.unwrap();

    let reconnected = tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reconnected.is_ok(),
        "an oversized unterminated response line must force a reconnect, not hang forever"
    );

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-58 comment finding 3: with no rate budget on the target's post-
/// login response reads, a misbehaving or MITM'd target sending an
/// endless stream of short, valid, newline-terminated lines could keep
/// the uplink task hot indefinitely. A flood past the budget must instead
/// tear the connection down.
#[tokio::test]
async fn a_flood_of_short_response_lines_past_the_rate_budget_forces_a_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, mut wr) = mock_rbn_accept_and_login(&listener).await;

    // Comfortably past the response-line rate budget within one window --
    // individually harmless lines, unbounded only in aggregate.
    for _ in 0..40 {
        wr.write_all(b"noise\r\n").await.unwrap();
    }

    let reconnected = tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reconnected.is_ok(),
        "a flood of response lines past the rate budget must force a reconnect"
    );

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn disabled_uplink_makes_no_connection_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let mut cfg = uplink_config(addr.port(), false);
    cfg.enabled = false;
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus, metrics, shutdown_rx).await;
    });

    let attempt = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(
        attempt.is_err(),
        "a disabled uplink must never attempt a connection"
    );
}

#[tokio::test]
async fn spot_is_forwarded_to_every_configured_target() {
    let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port1 = listener1.local_addr().unwrap().port();
    let port2 = listener2.local_addr().unwrap().port();

    let harness = spawn_uplinks(vec![
        uplink_config(port1, false),
        uplink_config(port2, false),
    ]);

    let (login1, mut reader1, _wr1) = mock_rbn_accept_and_login(&listener1).await;
    let (login2, mut reader2, _wr2) = mock_rbn_accept_and_login(&listener2).await;
    assert_eq!(login1.trim_end(), STATION_CALL);
    assert_eq!(login2.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(&spot, STATION_CALL, harness.bus.unix_ts_for(spot.sample_ts));
    harness.bus.publish(spot);

    let mut line1 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader1.read_line(&mut line1))
        .await
        .expect("timed out waiting for the forwarded spot on target 1")
        .unwrap();
    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader2.read_line(&mut line2))
        .await
        .expect("timed out waiting for the forwarded spot on target 2")
        .unwrap();

    assert_eq!(line1.trim_end(), expected);
    assert_eq!(line2.trim_end(), expected);

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn one_target_down_does_not_block_delivery_to_the_reachable_target_and_retries_independently()
{
    let listener_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_up = listener_up.local_addr().unwrap().port();

    // Bind then immediately drop to get a port nothing listens on, so the
    // second uplink task's connection attempts fail for the life of the test.
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_down = temp.local_addr().unwrap().port();
    drop(temp);

    let harness = spawn_uplinks(vec![
        uplink_config(port_up, false),
        uplink_config(port_down, false),
    ]);

    // Then the spot is still forwarded to the target that is reachable
    let (login_up, mut reader_up, _wr_up) = mock_rbn_accept_and_login(&listener_up).await;
    assert_eq!(login_up.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(&spot, STATION_CALL, harness.bus.unix_ts_for(spot.sample_ts));
    harness.bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader_up.read_line(&mut line))
        .await
        .expect("timed out waiting for the forwarded spot on the reachable target")
        .unwrap();
    assert_eq!(line.trim_end(), expected);

    // And the unreachable target's connection is retried independently --
    // wait for at least one reconnect attempt from the down target's own
    // backoff loop.
    tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the unreachable target's reconnect loop never attempted a retry");

    // The down target's failed attempts must not clear the shared
    // uplink_connected gauge while the reachable target is still up
    // (regression coverage: a shared last-writer-wins boolean would flip
    // this to false here even though the reachable target never dropped).
    assert!(
        harness.metrics.uplink_connected(),
        "an unrelated target's failed reconnects cleared the connected gauge"
    );

    // The reachable target's own delivery must be unaffected by the other
    // target's ongoing retries: forward a second spot and confirm it still
    // arrives on the same still-open connection.
    let spot2 = sample_spot();
    let expected2 = rbn::format_line(
        &spot2,
        STATION_CALL,
        harness.bus.unix_ts_for(spot2.sample_ts),
    );
    harness.bus.publish(spot2);
    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader_up.read_line(&mut line2))
        .await
        .expect("reachable target stopped receiving spots while the other target retried")
        .unwrap();
    assert_eq!(line2.trim_end(), expected2);

    let _ = harness.shutdown_tx.send(true);
}
