//! MAN-324: the live-state listener is `json_stream::serve` on the bus's
//! live channel -- same transport and fan-out rules as the spot stream,
//! its own channel, client cap, per-IP quota and loss counters.
//!
//! Scenario 4: "A slow subscriber never slows decoding" -- given a
//! subscriber that stops reading, when its buffer fills, then it is
//! dropped and publishing (the decode loop's side) is unaffected.

use futures_util::StreamExt;
use manta_server::bus::{SpotBus, LIVE_CHANNEL_CAPACITY};
use manta_server::json_stream::{StreamKind, MAX_LIVE_STREAM_CONNECTIONS};
use manta_server::metrics::Metrics;
use manta_spot::cty::Table;
use manta_spot::{Spot, SpotType};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
Japan:            25: 45: AS:  36.0: 138.0:  9.0:  JA:
    JA,JD,JE,JF,JG,JH,JI,JJ,JK,JL,JM,JN,JO,JP,JQ,JR,JS;
";

/// Spawns one `serve` listener of `stream` kind on `bus`.
async fn spawn_listener(
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    stream: StreamKind,
    shutdown: watch::Receiver<bool>,
    per_ip: usize,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let max = match stream {
        StreamKind::Spots => manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
        StreamKind::Live => MAX_LIVE_STREAM_CONNECTIONS,
    };
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus,
                metrics,
                lists: Arc::new(manta_spot::reference::ReferenceLists::new(
                    Arc::new(Table::parse(CTY_FIXTURE)),
                    Arc::new(manta_spot::scp::Set::bundled()),
                )),
                station_call: "W3XYZ".to_string(),
                station_location: None,
                decoder_version: "manta-test".to_string(),
                shutdown,
                drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                stream,
            },
            manta_server::tasks::new_client_tasks(),
            manta_server::tasks::new_connection_limiter(max),
            manta_server::tasks::IpQuota::new(per_ip),
            manta_server::rate_limit::IpRateLimiter::new(
                manta_server::json_stream::MAX_INBOUND_PINGS,
                manta_server::json_stream::PING_RATE_WINDOW,
            ),
        )
        .await;
    });
    addr
}

struct Node {
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    live: SocketAddr,
    spots: SocketAddr,
    _shutdown: watch::Sender<bool>,
}

async fn spawn_node(per_ip: usize) -> Node {
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    metrics.enable_live();
    let (tx, rx) = watch::channel(false);
    let live = spawn_listener(
        bus.clone(),
        metrics.clone(),
        StreamKind::Live,
        rx.clone(),
        per_ip,
    )
    .await;
    let spots = spawn_listener(bus.clone(), metrics.clone(), StreamKind::Spots, rx, per_ip).await;
    Node {
        bus,
        metrics,
        live,
        spots,
        _shutdown: tx,
    }
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

/// Waits until the bus has `n` live subscribers.
async fn wait_for_live_subscribers(bus: &SpotBus, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while bus.live_receiver_count() < n {
        assert!(
            Instant::now() < deadline,
            "live subscribers never reached {n}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn read_line(reader: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for a line")
        .unwrap();
    line
}

#[tokio::test]
async fn a_raw_tcp_live_client_receives_published_lines_verbatim() {
    let node = spawn_node(16).await;
    let mut reader = BufReader::new(TcpStream::connect(node.live).await.unwrap());
    wait_for_live_subscribers(&node.bus, 1).await;
    let record = r#"{"kind":"trackClosed","deCall":"W3XYZ","timestamp":1,"trackId":1,"closure":"signalEnded","survivorTrackId":null}"#;
    node.bus.publish_live(Arc::from(record));
    assert_eq!(read_line(&mut reader).await, format!("{record}\n"));
}

#[tokio::test]
async fn a_websocket_live_client_receives_one_text_frame_per_record() {
    let node = spawn_node(16).await;
    let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{}", node.live))
        .await
        .expect("ws connect failed");
    wait_for_live_subscribers(&node.bus, 1).await;
    for i in 0..3 {
        node.bus
            .publish_live(Arc::from(format!(r#"{{"kind":"bandState","n":{i}}}"#)));
    }
    for i in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("stream ended")
            .expect("ws error");
        assert_eq!(
            msg.into_text().unwrap().as_str(),
            format!(r#"{{"kind":"bandState","n":{i}}}"#)
        );
    }
}

#[tokio::test]
async fn the_spot_stream_never_carries_live_records_and_the_live_stream_never_carries_spots() {
    let node = spawn_node(16).await;
    let mut live = BufReader::new(TcpStream::connect(node.live).await.unwrap());
    let mut spots = BufReader::new(TcpStream::connect(node.spots).await.unwrap());
    let mut hello = String::new();
    tokio::time::timeout(Duration::from_secs(5), spots.read_line(&mut hello))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hello).unwrap()["kind"],
        "hello"
    );
    wait_for_live_subscribers(&node.bus, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    node.bus.publish_live(Arc::from(r#"{"kind":"bandState"}"#));
    node.bus.publish(sample_spot());
    node.bus.publish_live(Arc::from(r#"{"kind":"trackState"}"#));
    assert_eq!(read_line(&mut live).await, "{\"kind\":\"bandState\"}\n");
    assert_eq!(read_line(&mut live).await, "{\"kind\":\"trackState\"}\n");
    let spot: serde_json::Value = serde_json::from_str(&read_line(&mut spots).await).unwrap();
    assert_eq!(spot["dxCall"], "JA1ABC");
    // Nothing else arrives on either stream.
    let mut more = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), spots.read_line(&mut more))
            .await
            .is_err(),
        "the spot stream carried {more:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), live.read_line(&mut more))
            .await
            .is_err(),
        "the live stream carried {more:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_client_that_stops_reading_is_dropped_and_publishing_never_blocks() {
    let node = spawn_node(16).await;
    // A client that never reads, and one reading concurrently.
    let mut stalled = TcpStream::connect(node.live).await.unwrap();
    let reader_stream = TcpStream::connect(node.live).await.unwrap();
    wait_for_live_subscribers(&node.bus, 2).await;
    // Both classified as raw TCP (the server's 500 ms handshake peek)
    // before the burst starts.
    let deadline = Instant::now() + Duration::from_secs(5);
    while node.metrics.live_clients() < 2 {
        assert!(Instant::now() < deadline, "clients were never classified");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let total = LIVE_CHANNEL_CAPACITY + 20_000;
    // Lines the reading client has consumed, so the publisher can be paced
    // by that client's real progress rather than by wall-clock guesses.
    let consumed = Arc::new(AtomicUsize::new(0));
    let reader = tokio::spawn({
        let consumed = Arc::clone(&consumed);
        async move {
            let mut reader = BufReader::new(reader_stream);
            let mut got = 0usize;
            let mut line = String::new();
            while got < total {
                line.clear();
                let n = tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
                    .await
                    .expect("the reading client stalled")
                    .unwrap();
                assert!(
                    n > 0,
                    "the reading client was disconnected after {got} lines"
                );
                let idx: usize = line[..line.find(' ').unwrap()].parse().unwrap();
                assert_eq!(idx, got, "lines arrive in order");
                got += 1;
                consumed.store(got, Ordering::Release);
            }
            got
        }
    });

    // Pace the publisher like a decode loop handing over batches, keeping
    // it at most half the live channel ahead of what the reading client
    // has consumed. The server's handler for that client has received at
    // least as much as the client has read, so its receiver can never lag
    // -- however slow this host's loopback is -- while the stalled
    // client's handler, blocked on a full socket, falls far behind. The
    // wait happens here, outside `publish_live`, so it does not count as
    // publishing time below.
    let pad = "x".repeat(1024);
    let mut publishing = Duration::ZERO;
    for i in 0..total {
        let deadline = Instant::now() + Duration::from_secs(60);
        while i - consumed.load(Ordering::Acquire) >= LIVE_CHANNEL_CAPACITY / 2 {
            if reader.is_finished() {
                // It ended early: surface its panic message.
                reader.await.unwrap();
                panic!("the reading client ended after fewer than {total} lines");
            }
            assert!(
                Instant::now() < deadline,
                "the reading client made no progress for 60 s"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let line: Arc<str> = Arc::from(format!("{i} {pad}"));
        let start = Instant::now();
        node.bus.publish_live(line);
        publishing += start.elapsed();
    }
    // Publishing is a non-blocking broadcast send: all of the calls
    // together take far less than the server's 10 s per-write timeout that
    // a send waiting on the stalled client would have to sit through.
    assert!(
        publishing < Duration::from_secs(2),
        "publishing took {publishing:?}"
    );

    assert_eq!(reader.await.unwrap(), total);

    // The stalled socket, read now, reaches EOF: the server dropped it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        assert!(
            Instant::now() < deadline,
            "the stalled client was never dropped"
        );
        match tokio::time::timeout(Duration::from_secs(30), stalled.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(_)) => continue,
            Err(_) => panic!("the stalled client was never dropped"),
        }
    }
    assert!(node.metrics.live_records_dropped() > 0);
    let text = node.metrics.render_prometheus_text();
    assert!(
        text.contains("manta_spots_dropped_lagged_total 0\n"),
        "{text}"
    );
    assert_eq!(node.metrics.spots_dropped_write_failed_total(), 0);
    assert_eq!(node.metrics.spots_dropped_shutdown_total(), 0);
}

#[tokio::test]
async fn the_live_listener_admits_at_most_32_clients() {
    // A per-IP quota above the total cap, so only the cap binds.
    let node = spawn_node(1_000).await;
    let mut clients = Vec::new();
    for _ in 0..MAX_LIVE_STREAM_CONNECTIONS + 3 {
        clients.push(TcpStream::connect(node.live).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(node.bus.live_receiver_count(), MAX_LIVE_STREAM_CONNECTIONS);
    assert_eq!(
        node.metrics.live_clients(),
        MAX_LIVE_STREAM_CONNECTIONS as i64
    );
    // Freeing one slot admits a waiting client.
    drop(clients.remove(0));
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(node.bus.live_receiver_count(), MAX_LIVE_STREAM_CONNECTIONS);
}

#[tokio::test]
async fn the_live_per_ip_quota_is_counted_apart_from_the_spot_streams() {
    let node = spawn_node(2).await;
    // Fill the spot stream's per-IP quota...
    let _s1 = TcpStream::connect(node.spots).await.unwrap();
    let _s2 = TcpStream::connect(node.spots).await.unwrap();
    // ...and the live stream still admits its own two.
    let mut l1 = BufReader::new(TcpStream::connect(node.live).await.unwrap());
    let _l2 = TcpStream::connect(node.live).await.unwrap();
    wait_for_live_subscribers(&node.bus, 2).await;
    // A third live client from the same IP is declined (closed at once).
    let mut l3 = TcpStream::connect(node.live).await.unwrap();
    let mut buf = [0u8; 16];
    let closed = tokio::time::timeout(Duration::from_secs(5), l3.read(&mut buf)).await;
    assert!(matches!(closed, Ok(Ok(0)) | Ok(Err(_))), "{closed:?}");
    assert_eq!(node.bus.live_receiver_count(), 2);
    node.bus.publish_live(Arc::from("{}"));
    assert_eq!(read_line(&mut l1).await, "{}\n");
}

/// Codex P2 on #281: a peer that opens a WebSocket handshake is subscribed
/// on accept, so records queue for it; when the handshake is then
/// rejected, those records are charged to `write_failed`, not lost
/// silently.
#[tokio::test]
async fn records_queued_for_a_failed_websocket_handshake_are_counted_as_dropped() {
    use tokio::io::AsyncWriteExt;
    let node = spawn_node(16).await;
    let mut stream = TcpStream::connect(node.live).await.unwrap();
    // Classified as a WebSocket client, but no upgrade headers yet.
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();
    wait_for_live_subscribers(&node.bus, 1).await;
    for i in 0..3 {
        node.bus
            .publish_live(Arc::from(format!(r#"{{"kind":"bandState","n":{i}}}"#)));
    }
    // Completing a plain (non-upgrade) request makes the handshake fail.
    stream.write_all(b"\r\n").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while node.metrics.live_records_dropped() < 3 {
        assert!(
            Instant::now() < deadline,
            "{}",
            node.metrics.render_prometheus_text()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(node
        .metrics
        .render_prometheus_text()
        .contains("manta_live_records_dropped_total{reason=\"write_failed\"} 3\n"));
}

#[tokio::test]
async fn live_state_stream_sends_no_hello() {
    let node = spawn_node(16).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/?since=abc", node.live))
        .await
        .unwrap();
    node.bus.publish_live(Arc::from(r#"{"kind":"bandState"}"#));
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_text().unwrap(), r#"{"kind":"bandState"}"#);
}
