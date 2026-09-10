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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ";

async fn spawn_server() -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
    manta_server::tasks::ClientTasks,
) {
    spawn_server_with(
        rbn::LineFormat::Rbn,
        manta_server::tasks::CLIENT_DRAIN_DEADLINE,
    )
    .await
}

/// MAN-88: lets a test pick the wire layout (`rbn` vs `skimmer`) the server
/// renders spots with, on the production drain deadline.
async fn spawn_server_with_format(
    line_format: rbn::LineFormat,
) -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
    manta_server::tasks::ClientTasks,
) {
    spawn_server_with(line_format, manta_server::tasks::CLIENT_DRAIN_DEADLINE).await
}

/// MAN-45 (PR #63 round-16 finding): lets a test drive the per-client
/// shutdown-drain deadline directly (e.g. `Duration::ZERO`, to make an
/// expiry exact rather than timing-dependent) instead of always waiting on
/// the production `CLIENT_DRAIN_DEADLINE`.
async fn spawn_server_with_drain_deadline(
    drain_deadline: Duration,
) -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
    manta_server::tasks::ClientTasks,
) {
    spawn_server_with(rbn::LineFormat::Rbn, drain_deadline).await
}

async fn spawn_server_with(
    line_format: rbn::LineFormat,
    drain_deadline: Duration,
) -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
    manta_server::tasks::ClientTasks,
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
    // Kept alongside the clone moved into `serve()` (PR #75 review, round
    // 3) so a caller can directly inspect a client task's own join result
    // afterward -- a panicking task also drops its socket, so observing
    // the socket close alone can't distinguish a clean disconnect from a
    // handler panic.
    let tasks_handle = tasks.clone();
    let limiter =
        manta_server::tasks::new_connection_limiter(manta_server::telnet::MAX_TELNET_CONNECTIONS);
    tokio::spawn(async move {
        manta_server::telnet::serve(
            listener,
            bus2,
            metrics2,
            STATION_CALL.to_string(),
            shutdown_rx,
            tasks,
            limiter,
            manta_server::tasks::IpQuota::new(manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP),
            manta_server::rate_limit::IpRateLimiter::new(
                manta_server::telnet::MAX_TELNET_COMMANDS,
                manta_server::telnet::COMMAND_RATE_WINDOW,
            ),
            line_format,
            drain_deadline,
        )
        .await;
    });

    (addr, bus, metrics, shutdown_tx, tasks_handle)
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;

    // History predates the client's connection entirely -- `sh/dx` reads
    // the bus's retained history, not the live broadcast subscription.
    let mut first = sample_spot();
    first.callsign = "K5ARH".to_string();
    let mut second = sample_spot();
    second.callsign = "N0CALL".to_string();
    let expected_first = rbn::format_line(
        &first,
        STATION_CALL,
        bus.unix_ts_for(first.sample_ts),
        rbn::LineFormat::Rbn,
    );
    let expected_second = rbn::format_line(
        &second,
        STATION_CALL,
        bus.unix_ts_for(second.sample_ts),
        rbn::LineFormat::Rbn,
    );
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;

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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
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
    let (addr, bus, _metrics, shutdown_tx, _tasks) = spawn_server().await;
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

/// Validation round 17 (CR-1): before this fix, `handle_client`'s pre-loop
/// login handshake (prompt write, login-line read, banner write) never
/// observed `shutdown` at all -- a client that received the prompt and then
/// simply stalled without sending a login line held its task inside
/// `read_line_bounded_with_timeout` for up to `bounded_io::IDLE_READ_TIMEOUT`
/// (30s), invisible to shutdown the whole time. The fix races every step of
/// the handshake against `shutdown.changed()`. This is fully deterministic
/// (no timing race): the "client" here never sends anything after reading
/// the prompt, so the ONLY way the connection can close is via the new
/// shutdown-aware branch -- without the fix this test would hang until the
/// assertion's own timeout fires.
///
/// MAN-45 remediate (code-review round 18, finding 2): also asserts the
/// abandoned backlog lands on `spots_dropped_shutdown_total`, NOT
/// `spots_dropped_write_failed_total` -- no write ever failed on this path,
/// the daemon shut down cleanly, and conflating the two would mislead an
/// operator reading `spots_dropped_write_failed_total`'s own "socket write
/// timed out or failed" HELP text.
#[tokio::test]
async fn shutdown_during_login_handshake_disconnects_promptly_instead_of_idling_out() {
    let (addr, bus, metrics, shutdown_tx, _tasks) = spawn_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Read (and discard) the login prompt, then go silent -- exactly a
    // client that connects and never logs in.
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("expected the login prompt")
        .unwrap();
    assert!(n > 0, "expected a non-empty login prompt");

    // Queued on this client's subscribed `rx` while it's stalled at the
    // login prompt, so the abandoned-backlog count below is provably
    // nonzero rather than a vacuous 0 == 0.
    bus.publish(sample_spot());
    bus.publish(sample_spot());

    let _ = shutdown_tx.send(true);

    let mut trailing = [0u8; 1];
    let read_result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut trailing))
        .await
        .expect(
            "a client stalled in the pre-login handshake must disconnect promptly once \
             shutdown is signalled, not wait out the idle-read timeout",
        );
    assert_eq!(
        read_result.unwrap(),
        0,
        "expected EOF after shutdown during the login handshake"
    );

    assert_eq!(
        metrics.spots_dropped_shutdown_total(),
        2,
        "the two queued spots must be charged to the shutdown counter"
    );
    assert_eq!(
        metrics.spots_dropped_write_failed_total(),
        0,
        "no write ever failed on this path -- it must not inflate the write-failure counter"
    );
}

/// MAN-45 (PR #63 round-16 finding): a drain that cannot finish inside its
/// own deadline must COUNT everything it abandons, never truncate silently
/// (ARCHITECTURE §8). Driven with a zero deadline so the expiry is exact
/// rather than timing-dependent -- the property under test is the
/// accounting, not the duration.
///
/// The assertion is the invariant, not a fixed split: `select!` may still
/// deliver some spots through the LIVE arm before the shutdown arm wins, so
/// what must hold is that every published spot is either delivered or
/// counted, never neither.
#[tokio::test]
async fn shutdown_drain_deadline_counts_the_backlog_it_could_not_write() {
    let (addr, bus, metrics, shutdown_tx, _tasks) =
        spawn_server_with_drain_deadline(Duration::ZERO).await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    // Published and signalled without an intervening await, so the client
    // task first wakes with all three already queued AND shutdown set.
    for _ in 0..3 {
        bus.publish(sample_spot());
    }
    let _ = shutdown_tx.send(true);

    // Read to EOF: the connection must close, not hang.
    let mut delivered = 0usize;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => delivered += 1,
            }
        }
    })
    .await;

    assert_eq!(
        delivered + metrics.spots_dropped_write_failed_total() as usize,
        3,
        "every published spot must be delivered or counted, never neither",
    );
}

/// MAN-45 (PR #63 round-19 P1 review finding, re-raised against an earlier
/// head): once shutdown is pending, a backlogged client must not keep
/// winning the live-spot arm. `tokio::select!` picks a RANDOM ready arm, so
/// before the `if !shutdown.has_changed()` preconditions landed on every
/// write-capable arm (`telnet::handle_client`'s live-spot and command-read
/// arms, `json_stream`'s TCP and WS equivalents) a client with a backlog
/// could perform an UNBOUNDED number of two-`WRITE_TIMEOUT` live writes
/// after shutdown was signalled and before its own `CLIENT_DRAIN_DEADLINE`
/// clock ever started -- which `SHUTDOWN_DRAIN_DEADLINE`'s
/// `2 * WRITE_TIMEOUT + CLIENT_DRAIN_DEADLINE` model (manta-cli's `main.rs`)
/// cannot cover at any constant value.
///
/// The invariant is AT MOST ONE live write after shutdown, not zero: the
/// preconditions are evaluated when `select!` is entered, so a handler
/// already parked in `select!` when shutdown fires can still take the
/// live-spot arm once (both arms are ready and the pick is random) before
/// the next trip through the loop disables it for good. One is exactly what
/// the outer deadline budgets for.
///
/// Repeated independent trials because `select!` picks a random ready arm,
/// so one trial only samples one coin flip. Measured honesty note, from
/// running this test against a build with both of `handle_client`'s
/// preconditions deleted: that build ALSO stays within the bound here
/// (`delivered` came out 0 or 1 in every trial, the same distribution the
/// fixed build produces). Reaching two or more post-shutdown live writes
/// needs writes slow enough to matter -- a client that has stopped reading,
/// so each write runs against `WRITE_TIMEOUT` -- which is a tens-of-seconds
/// test this suite deliberately does not carry. So this locks the observable
/// invariant the outer `SHUTDOWN_DRAIN_DEADLINE` model depends on (at most
/// one live write precedes the drain, and every published spot is either
/// delivered or counted); it is a guard against that invariant regressing,
/// not a demonstration that the preconditions are load-bearing in this
/// fast-write scenario.
#[tokio::test]
async fn shutdown_bounds_live_writes_to_at_most_one_before_the_drain() {
    const TRIALS: usize = 16;
    const QUEUED: usize = 4;

    for trial in 0..TRIALS {
        // Zero drain deadline so the trial resolves immediately: whatever
        // the live arm did NOT write is abandoned and counted rather than
        // slowly written out, which is what makes `delivered` the exact
        // count of post-shutdown LIVE writes.
        let (addr, bus, metrics, shutdown_tx, _tasks) =
            spawn_server_with_drain_deadline(Duration::ZERO).await;
        let (mut reader, _wr) = connect_and_login(addr).await;

        // Published and signalled without an intervening await (this test
        // runs on the current-thread runtime), so the client task first
        // wakes with the whole backlog queued AND shutdown already set --
        // the exact interleaving the finding describes.
        for _ in 0..QUEUED {
            bus.publish(sample_spot());
        }
        let _ = shutdown_tx.send(true);

        let mut delivered = 0usize;
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => delivered += 1,
                }
            }
        })
        .await;

        assert!(
            delivered <= 1,
            "trial {trial}: at most one live spot write may precede the shutdown drain \
             (the outer SHUTDOWN_DRAIN_DEADLINE budgets for exactly one), got {delivered}",
        );
        assert_eq!(
            delivered
                + metrics.spots_dropped_write_failed_total() as usize
                + metrics.spots_dropped_shutdown_total() as usize,
            QUEUED,
            "trial {trial}: every published spot must be delivered or counted, never neither",
        );
    }
}

#[tokio::test]
async fn connecting_client_is_counted_in_metrics() {
    let (addr, _bus, metrics, _shutdown_tx, _tasks) = spawn_server().await;
    let (_reader, _wr) = connect_and_login(addr).await;

    // Give the accept/login task a moment to run and increment the gauge.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(metrics
        .render_prometheus_text()
        .contains("manta_telnet_clients_connected 1"));
}

/// MAN-61: `ConnectionLimiter` alone bounds only the total connection
/// ceiling, not what one source can hold of it -- a source that opens
/// `MAX_TELNET_CONNECTIONS_PER_IP` connections and stays logged in with
/// nothing further to say (real, legitimate telnet client behavior --
/// module docs) must have its NEXT connection attempt declined, without
/// disturbing the ones it already holds.
#[tokio::test]
async fn a_source_past_its_per_ip_connection_cap_is_declined_without_disturbing_its_existing_connections(
) {
    let (addr, bus, metrics, _shutdown_tx, _tasks) = spawn_server().await;

    let mut clients = Vec::new();
    for _ in 0..manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP {
        clients.push(connect_and_login(addr).await);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        metrics.render_prometheus_text().contains(&format!(
            "manta_telnet_clients_connected {}",
            manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP
        )),
        "all {} connections from this source must have been admitted",
        manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP
    );

    // One more from the SAME source (127.0.0.1, like every connection in
    // this test) must be declined outright: the socket closes with no
    // login prompt at all, rather than being admitted and then later
    // disconnected.
    let mut extra = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 1];
    let read_result = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut buf))
        .await
        .expect("a declined connection must close promptly, not hang");
    assert_eq!(
        read_result.unwrap(),
        0,
        "a source past its per-IP cap must get EOF (no login prompt), not be admitted"
    );

    // The existing, already-admitted connections must be completely
    // unaffected by the decline -- still logged in and still receiving
    // spots normally.
    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
    bus.publish(spot);
    for (reader, _wr) in clients.iter_mut() {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("an already-admitted client must be unaffected by a later decline")
            .unwrap();
        assert_eq!(line.trim_end(), expected);
    }
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
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
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

#[tokio::test]
async fn a_line_with_no_newline_past_the_max_length_is_disconnected_not_crashed() {
    // MAN-22 acceptance: an unterminated line past `bounded_io::MAX_LINE_BYTES`
    // is rejected as a protocol violation (bounded_io.rs is unit-tested for
    // this directly), but that alone doesn't prove the real telnet server
    // wires the bound through end-to-end -- an oversized line could in
    // principle still grow an internal buffer, wedge the connection, or
    // take the whole listener down before the accept loop's own error
    // handling ever sees it. Send one over a real socket and confirm: the
    // offending connection is cleanly closed, and the server (and other
    // clients) keep working afterward.
    let (addr, bus, _metrics, _shutdown_tx, tasks) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    let oversized = vec![b'A'; manta_server::bounded_io::MAX_LINE_BYTES + 1];
    wr.write_all(&oversized).await.unwrap();
    // Deliberately never send a newline -- this is the "no terminator at
    // all" shape bounded_io.rs's unit test also covers.

    let mut trailing = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut trailing))
        .await
        .expect("server must actively close an oversized-line connection, not hang")
        .unwrap_or(0);
    assert_eq!(
        n, 0,
        "expected the offending connection to be closed, got: {trailing:?}"
    );

    // The socket closing is consistent with EITHER a clean protocol-error
    // disconnect OR a panic in the client-handler task (dropping the task
    // also drops the socket) -- so it alone doesn't distinguish them
    // (PR #75 review, round 3). Directly inspect the tracked task's own
    // join result, before spawning the second client below so there's no
    // ambiguity about which task's result this is.
    let join_result = tokio::time::timeout(Duration::from_secs(5), async {
        tasks.lock().await.join_next().await
    })
    .await
    .expect("oversized-line client handler task did not complete in time")
    .expect("client handler task set was unexpectedly empty");
    if let Err(join_err) = join_result {
        assert!(
            !join_err.is_panic(),
            "client handler task panicked on an oversized line: {join_err}"
        );
    }

    // The server itself must still be healthy: a fresh client can connect,
    // log in, and receive a spot.
    let (mut reader2, _wr2) = connect_and_login(addr).await;
    let spot = sample_spot();
    bus.publish(spot);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader2.read_line(&mut line))
        .await
        .expect("server must still serve other clients after an oversized line")
        .unwrap();
    assert!(line.contains("DX de"), "line was: {line:?}");
}

#[tokio::test]
async fn client_flooding_commands_past_the_rate_budget_is_disconnected() {
    // Regression test (round-14 review): an established (logged-in)
    // client could previously send an unlimited sequence of complete
    // commands with no rate or lifetime budget -- each `set dx filter`
    // (or worse, repeated `sh/dx/50`) is real CPU/bandwidth work. A
    // client must be disconnected once it exceeds a small per-window
    // command budget.
    let (addr, _bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
    let (mut reader, mut wr) = connect_and_login(addr).await;

    for i in 0..manta_server::telnet::MAX_TELNET_COMMANDS {
        wr.write_all(b"set dx filter unique > 1\r\n").await.unwrap();
        let mut ack = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ack))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for ack #{i} within the budget"))
            .unwrap_or_else(|_| panic!("read error waiting for ack #{i} within the budget"));
        assert!(
            ack.to_lowercase().contains("filter"),
            "expected a filter ack within budget, got: {ack:?}"
        );
    }

    // One more command, past the budget, must end the connection instead
    // of getting another ack.
    wr.write_all(b"set dx filter unique > 1\r\n").await.unwrap();
    let mut extra = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut extra))
        .await
        .expect("server never responded after the command budget was exceeded")
        .unwrap_or(0);
    assert_eq!(
        n, 0,
        "expected the connection to close after exceeding the command budget, got: {extra:?}"
    );
}

#[tokio::test]
async fn a_second_connection_from_the_same_ip_cannot_multiply_the_command_rate_budget() {
    // MAN-57: the per-connection RateLimiter alone let a source multiply
    // its effective command rate by opening more connections -- each one
    // got its own full independent budget. A second connection from the
    // SAME source IP must draw against the same shared aggregate budget
    // as the first, not get a fresh one of its own.
    let (addr, _bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
    let (mut reader_a, mut wr_a) = connect_and_login(addr).await;
    let (mut reader_b, mut wr_b) = connect_and_login(addr).await;

    // Connection A alone consumes the ENTIRE shared per-source budget --
    // each ack proves the command was accepted, well within what a lone
    // connection's own per-connection budget would also allow.
    for i in 0..manta_server::telnet::MAX_TELNET_COMMANDS {
        wr_a.write_all(b"set dx filter unique > 1\r\n")
            .await
            .unwrap();
        let mut ack = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader_a.read_line(&mut ack))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for ack #{i} on connection A"))
            .unwrap_or_else(|_| panic!("read error waiting for ack #{i} on connection A"));
        assert!(
            ack.to_lowercase().contains("filter"),
            "expected a filter ack within budget on connection A, got: {ack:?}"
        );
    }

    // Connection B is a FRESH connection with its own untouched
    // per-connection RateLimiter, so under the old (per-connection-only)
    // behavior this command would succeed. It must instead be rejected,
    // because the shared per-IP budget A already exhausted is checked
    // too.
    wr_b.write_all(b"set dx filter unique > 1\r\n")
        .await
        .unwrap();
    let mut extra = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader_b.read_line(&mut extra))
        .await
        .expect("server never responded on connection B after the shared budget was exhausted")
        .unwrap_or(0);
    assert_eq!(
        n, 0,
        "expected connection B to be disconnected once the SHARED per-IP budget \
         (already exhausted by connection A) was exceeded, got: {extra:?}"
    );
}

/// MAN-88 Scenario 1, end to end: the bytes on the wire, not just the
/// renderer's return value. Asserts against literal column numbers, so a
/// regression in any layer between `format_line` and the socket is caught --
/// the other tests in this file build their expectation by calling
/// `format_line` themselves and would not.
#[tokio::test]
async fn a_live_spot_arrives_in_the_fixed_column_ak1a_layout() {
    let (addr, bus, _metrics, _shutdown_tx, _tasks) = spawn_server().await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    let spot = sample_spot();
    let unix_ts = bus.unix_ts_for(spot.sample_ts);
    bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for spot line")
        .unwrap();
    let line = line.trim_end();

    let secs_of_day = unix_ts.rem_euclid(86_400);
    let zulu = format!("{:02}{:02}Z", secs_of_day / 3600, (secs_of_day % 3600) / 60);
    assert_eq!(line.find(&zulu).unwrap() + 1, 71, "line was: {line:?}");
    assert_eq!(line.find("CW").unwrap() + 1, 42, "line was: {line:?}");
    assert!(line.contains("14027.10"), "line was: {line:?}");
}

/// MAN-88 Scenario 2, end to end.
#[tokio::test]
async fn a_skimmer_mode_server_emits_the_no_mode_column_layout() {
    let (addr, bus, _metrics, _shutdown_tx, _tasks) =
        spawn_server_with_format(rbn::LineFormat::Skimmer).await;
    let (mut reader, _wr) = connect_and_login(addr).await;

    let spot = sample_spot();
    let unix_ts = bus.unix_ts_for(spot.sample_ts);
    bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for spot line")
        .unwrap();
    let line = line.trim_end();

    assert!(
        !line.contains(" CW "),
        "mode column still present: {line:?}"
    );
    let secs_of_day = unix_ts.rem_euclid(86_400);
    let zulu = format!("{:02}{:02}Z", secs_of_day / 3600, (secs_of_day % 3600) / 60);
    assert_eq!(line.find(&zulu).unwrap() + 1, 67, "line was: {line:?}");
}

/// The `sh/dx` replay path renders through the same function -- confirm it
/// honours the configured format rather than defaulting.
#[tokio::test]
async fn sh_dx_history_also_honours_the_configured_line_format() {
    let (addr, bus, _metrics, _shutdown_tx, _tasks) =
        spawn_server_with_format(rbn::LineFormat::Skimmer).await;
    let spot = sample_spot();
    let unix_ts = bus.unix_ts_for(spot.sample_ts);
    bus.publish(spot);

    let (mut reader, mut wr) = connect_and_login(addr).await;
    wr.write_all(b"sh/dx\r\n").await.unwrap();

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for history line")
        .unwrap();
    let line = line.trim_end();

    assert!(!line.contains(" CW "), "line was: {line:?}");
    // Assert the positive property too, not just the absence of the mode
    // column: a banner, a blank line or an error string would satisfy the
    // negative on its own.
    let secs_of_day = unix_ts.rem_euclid(86_400);
    let zulu = format!("{:02}{:02}Z", secs_of_day / 3600, (secs_of_day % 3600) / 60);
    assert_eq!(
        line.find(&zulu).map(|i| i + 1),
        Some(67),
        "line was: {line:?}"
    );
}
