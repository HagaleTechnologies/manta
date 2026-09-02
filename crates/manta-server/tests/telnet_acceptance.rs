//! MAN-12 acceptance scenario 1:
//!   Given manta has decoded and validated a CW spot
//!   When a telnet DX-cluster client connects to manta's cluster port and logs in
//!   Then it receives the spot in standard "DX de" RBN format

use manta_server::bus::SpotBus;
use manta_server::metrics::Metrics;
use manta_server::rbn;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ";

async fn spawn_server() -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
) {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let bus2 = bus.clone();
    let metrics2 = metrics.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    tokio::spawn(async move {
        manta_server::telnet::serve(
            listener,
            bus2,
            metrics2,
            STATION_CALL.to_string(),
            shutdown_rx,
            tasks,
        )
        .await;
    });

    (addr, bus, metrics, shutdown_tx)
}

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

async fn connect_and_login(
    addr: std::net::SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    let mut prompt = String::new();
    reader.read_line(&mut prompt).await.unwrap();
    assert!(
        prompt.to_lowercase().contains("login") || prompt.to_lowercase().contains("call"),
        "expected a login prompt, got: {prompt:?}"
    );

    wr.write_all(b"N0CALL\r\n").await.unwrap();

    // Consume the post-login greeting line(s) up through the station's
    // own prompt (`de W3XYZ-# >`) before the spot stream starts.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line.contains(STATION_CALL) {
            break;
        }
    }

    (reader, wr)
}

#[tokio::test]
async fn standard_client_receives_spot_in_rbn_format_after_login() {
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    let spot = sample_spot();
    let expected = rbn::format_line(&spot, STATION_CALL, bus.unix_ts_for(spot.sample_ts));
    bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for spot line")
        .unwrap();

    assert_eq!(line.trim_end(), expected);
}

#[tokio::test]
async fn sh_dx_command_does_not_disconnect_the_client() {
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    wr.write_all(b"sh/dx\r\n").await.unwrap();

    // The connection must survive an unrecognized/read-only command --
    // a spot published afterward must still arrive.
    let spot = sample_spot();
    bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("connection did not survive sh/dx")
        .unwrap();
    assert!(line.contains("DX de"), "line was: {line:?}");
}

#[tokio::test]
async fn sh_dx_replays_recent_spot_history_in_rbn_format() {
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;

    // History predates the client's connection entirely -- `sh/dx` reads
    // the bus's retained history, not the live broadcast subscription.
    let mut first = sample_spot();
    first.callsign = "K5ARH".to_string();
    let mut second = sample_spot();
    second.callsign = "N0CALL".to_string();
    let expected_first = rbn::format_line(&first, STATION_CALL, bus.unix_ts_for(first.sample_ts));
    let expected_second =
        rbn::format_line(&second, STATION_CALL, bus.unix_ts_for(second.sample_ts));
    bus.publish(first);
    bus.publish(second);

    let (mut reader, mut wr) = connect_and_login(addr).await;
    wr.write_all(b"sh/dx\r\n").await.unwrap();

    let mut line1 = String::new();
    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line1))
        .await
        .expect("timed out waiting for first history line")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line2))
        .await
        .expect("timed out waiting for second history line")
        .unwrap();

    assert_eq!(line1.trim_end(), expected_first);
    assert_eq!(line2.trim_end(), expected_second);
}

#[tokio::test]
async fn sh_dx_history_replay_honors_the_unique_filter() {
    // Regression test (round-11 review): a spot suppressed on the live
    // stream by `set dx filter unique > n` must stay suppressed when the
    // same client replays it via `sh/dx` -- the filter must apply
    // consistently to both paths, not just the live one.
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;

    // Published BEFORE the client connects, so this is pure history --
    // sh/dx's replay path, not the live broadcast path.
    let mut first = sample_spot();
    first.callsign = "K5ARH".to_string();
    let mut second = sample_spot();
    second.callsign = "K5ARH".to_string();
    bus.publish(first); // occurrence 1: must be filtered out of history
    bus.publish(second); // occurrence 2: must survive the filter

    let (mut reader, mut wr) = connect_and_login(addr).await;
    wr.write_all(b"set dx filter unique > 1\r\n").await.unwrap();
    let mut ack = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ack))
        .await
        .expect("timed out waiting for filter ack")
        .unwrap();

    wr.write_all(b"sh/dx\r\n").await.unwrap();

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the surviving history line")
        .unwrap();
    assert!(
        line.contains("K5ARH"),
        "the second (unfiltered) occurrence must still appear: {line:?}"
    );

    // Only one history line should have arrived -- the first (filtered)
    // occurrence must not also show up.
    let mut extra = String::new();
    let extra_result =
        tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut extra)).await;
    assert!(
        extra_result.is_err(),
        "the filtered-out first occurrence must not appear in sh/dx history: {extra:?}"
    );
}

#[tokio::test]
async fn set_dx_filter_unique_suppresses_below_threshold_occurrences() {
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    wr.write_all(b"set dx filter unique > 1\r\n").await.unwrap();
    let mut ack = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ack))
        .await
        .expect("timed out waiting for filter ack")
        .unwrap();
    assert!(ack.to_lowercase().contains("filter"), "ack was: {ack:?}");

    let mut spot = sample_spot();
    spot.callsign = "K5ARH".to_string();

    // First occurrence: occurrence_count becomes 1, filter requires > 1,
    // so this must NOT be forwarded.
    bus.publish(spot.clone());
    let mut suppressed = String::new();
    let first_result = tokio::time::timeout(
        Duration::from_millis(300),
        reader.read_line(&mut suppressed),
    )
    .await;
    assert!(
        first_result.is_err(),
        "first occurrence should have been filtered out, got: {suppressed:?}"
    );

    // Second occurrence: occurrence_count becomes 2, which clears the
    // > 1 threshold, so this one must arrive.
    bus.publish(spot);
    let mut forwarded = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut forwarded))
        .await
        .expect("second occurrence should have been forwarded")
        .unwrap();
    assert!(forwarded.contains("K5ARH"), "line was: {forwarded:?}");
}

#[tokio::test]
async fn filter_evaluates_each_spot_at_its_own_publication_time_not_drain_time() {
    // Regression test: two occurrences published back-to-back BEFORE the
    // client's task drains either one. `occurrence_count` must reflect
    // what each spot had at ITS OWN publish, not the running total by the
    // time the client gets around to checking it -- otherwise both the
    // first (which should be suppressed) and second occurrence would pass
    // a `unique > 1` filter once the count had already reached 2.
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    wr.write_all(b"set dx filter unique > 1\r\n").await.unwrap();
    let mut ack = String::new();
    reader.read_line(&mut ack).await.unwrap();

    let mut spot = sample_spot();
    spot.callsign = "K5ARH".to_string();
    bus.publish(spot.clone()); // occurrence 1: must be suppressed
    bus.publish(spot); // occurrence 2: must be forwarded

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the second occurrence")
        .unwrap();
    assert!(line.contains("K5ARH"), "line was: {line:?}");

    // Exactly one line should have arrived -- the first occurrence must
    // not also show up as a second forwarded line.
    let mut extra = String::new();
    let extra_result =
        tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut extra)).await;
    assert!(
        extra_result.is_err(),
        "the suppressed first occurrence must not also arrive: {extra:?}"
    );
}

#[tokio::test]
async fn a_command_split_across_writes_survives_a_spot_arriving_mid_command() {
    // Regression test: the command line ("sh/dx") arrives in two TCP
    // writes with a live spot published in between -- forcing the
    // server's `tokio::select!` to cancel the in-progress read for the
    // spot branch, then resume reading the command's remainder. The full
    // command must still be recognized, not corrupted into "x" (parsed as
    // Command::Unknown, silently producing no history replay at all).
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    let mut history_spot = sample_spot();
    history_spot.callsign = "N0CALL".to_string();
    bus.publish(history_spot);

    wr.write_all(b"sh/d").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await; // let the server start awaiting more

    let live_spot = sample_spot();
    bus.publish(live_spot); // races the in-progress command read

    tokio::time::sleep(Duration::from_millis(50)).await;
    wr.write_all(b"x\r\n").await.unwrap(); // completes "sh/dx"

    // Collect whatever arrives over a bounded window -- the live spot's
    // broadcast delivery and the sh/dx history reply can interleave in
    // either order; what matters is that N0CALL (only ever reachable via
    // a correctly-reassembled "sh/dx", never via "x" parsed as
    // Command::Unknown) shows up at all.
    let mut lines = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            lines.push(line);
        }
    })
    .await;

    assert!(
        lines.iter().any(|l| l.contains("N0CALL")),
        "sh/dx must have been recognized, not corrupted into an unknown command; got: {lines:?}"
    );
}

#[tokio::test]
async fn shutdown_drains_an_already_queued_spot_before_disconnecting() {
    // Regression test: a spot published right as the daemon shuts down
    // (e.g. from TrackManager::finish() just before exit) must still
    // reach the client, not be dropped when the runtime tears the
    // connection's task down.
    let (addr, bus, _metrics, shutdown_tx) = spawn_server().await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    bus.publish(sample_spot());
    let _ = shutdown_tx.send(true);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("a spot published right at shutdown must not be lost")
        .unwrap();
    assert!(line.contains("DX de"), "line was: {line:?}");

    let mut trailing = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut trailing))
        .await
        .expect("connection never closed after the shutdown drain")
        .unwrap();
    assert_eq!(n, 0, "expected EOF after shutdown drain, got: {trailing:?}");
}

#[tokio::test]
async fn connecting_client_is_counted_in_metrics() {
    let (addr, _bus, metrics, _shutdown_tx) = spawn_server().await;
    let (_reader, _wr) = connect_and_login(addr).await;

    // Give the accept/login task a moment to run and increment the gauge.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(metrics
        .render_prometheus_text()
        .contains("manta_telnet_clients_connected 1"));
}

#[tokio::test]
async fn a_logged_in_client_that_sends_no_commands_survives_past_the_login_idle_timeout() {
    // Regression test (round-5 review, telnet.rs:117): a real telnet
    // client is read-mostly -- it may sit logged in for minutes with
    // nothing to say while just watching spots. The idle-read timeout must
    // guard login (a client that never sends a callsign) and an
    // in-progress partial command, NOT the steady-state "waiting for the
    // next command" state -- disconnecting a quiet-but-healthy client
    // after `bounded_io::IDLE_READ_TIMEOUT` was the bug.
    //
    // This needs a genuine wall-clock wait past that deadline: mixing
    // `tokio::time::pause`/`advance` with this test's real TCP sockets
    // (tried first) made the server reset every connection outright --
    // paused-time auto-advance doesn't coexist safely with real socket
    // I/O on this runtime, so a real (if slow) wait is the trustworthy
    // option here.
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    tokio::time::sleep(manta_server::bounded_io::IDLE_READ_TIMEOUT + Duration::from_secs(1)).await;

    // The connection must still be alive: a spot published after crossing
    // the old timeout threshold must still be delivered, not met with a
    // connection the server already closed out from under the client.
    let spot = sample_spot();
    bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("client must still be connected past the old login-only idle timeout")
        .unwrap();
    assert!(line.contains("DX de"), "line was: {line:?}");
}
