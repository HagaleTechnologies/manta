//! MAN-12 acceptance scenario 2:
//!   Given manta has decoded and validated a CW spot
//!   When a client connects to manta's JSON/WebSocket stream
//!   Then it receives the same spot as a JSON Lines message matching the
//!   agreed cqdx contract (spots.v1.schema.json)

use futures_util::StreamExt;
use manta_server::bus::SpotBus;
use manta_server::metrics::Metrics;
use manta_spot::cty::Table;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ";
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
Japan:            25: 45: AS:  36.0: 138.0:  9.0:  JA:
    JA,JD,JE,JF,JG,JH,JI,JJ,JK,JL,JM,JN,JO,JP,JQ,JR,JS;
";

async fn spawn_server() -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    Arc<Metrics>,
    tokio::sync::watch::Sender<bool>,
) {
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    let cty = Arc::new(Table::parse(CTY_FIXTURE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let bus2 = bus.clone();
    let metrics2 = metrics.clone();
    let tasks = manta_server::tasks::new_client_tasks();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus: bus2,
                metrics: metrics2,
                cty,
                station_call: STATION_CALL.to_string(),
                decoder_version: "manta-test".to_string(),
                shutdown: shutdown_rx,
            },
            tasks,
            limiter,
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

#[tokio::test]
async fn tcp_client_receives_spot_as_json_lines_message() {
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut reader = BufReader::new(stream);

    // Give the server task a moment to accept + subscribe before the
    // spot is published (see the telnet server's subscribe-before-login
    // ordering fix for why this matters with a broadcast channel).
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(sample_spot());

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for JSON line")
        .unwrap();

    let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(value["source"], "skimmer");
    assert_eq!(value["mode"], "CW");
    assert_eq!(value["dxCall"], "JA1ABC");
    assert_eq!(value["deCall"], "W3XYZ");
    assert_eq!(value["band"], "20m");
    assert_eq!(value["frequency"], 14_027_100);
    assert_eq!(value["dxContinent"], "AS");
    assert_eq!(value["dxCqZone"], 25);
    assert!(value["dxDxcc"].is_null());
}

#[tokio::test]
async fn shutdown_drains_an_already_queued_spot_before_disconnecting() {
    // Regression test: a spot published right as the daemon shuts down
    // must still reach the client, not be dropped when the runtime tears
    // the connection's task down.
    let (addr, bus, _metrics, shutdown_tx) = spawn_server().await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut reader = BufReader::new(stream);
    tokio::time::sleep(Duration::from_millis(600)).await; // past the WS-detection peek window

    bus.publish(sample_spot());
    let _ = shutdown_tx.send(true);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("a spot published right at shutdown must not be lost")
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(value["dxCall"], "JA1ABC");
}

#[tokio::test]
async fn tcp_client_close_during_a_quiet_period_is_detected() {
    // Regression test: a raw JSON-lines client that disconnects while no
    // spots are being published must not leave its task/socket/gauge
    // alive until some later spot's write happens to fail.
    let (addr, _bus, metrics, _shutdown_tx) = spawn_server().await;

    let stream = TcpStream::connect(addr).await.unwrap();
    // The WS-detection peek can wait up to PEEK_TIMEOUT (500ms) before
    // classifying a client that sends nothing as plain TCP and
    // incrementing the gauge -- poll instead of a fixed short sleep.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if metrics
                .render_prometheus_text()
                .contains("manta_json_clients_connected 1")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("client was never classified/counted as a JSON client");

    drop(stream); // client closes the connection, no spot involved at all

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if metrics
                .render_prometheus_text()
                .contains("manta_json_clients_connected 0")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("client close was never detected during the quiet period");
}

#[tokio::test]
async fn tcp_client_sending_data_is_disconnected_not_looped_on() {
    // Regression test (round-5 review): this protocol is pure server push
    // -- a raw TCP client is never expected to send anything. The old
    // behavior discarded any client data and looped back to read again,
    // which under a client that keeps streaming data makes the read
    // branch of `handle_tcp_client`'s select! perpetually ready, burning
    // CPU in a tight loop instead of ever disconnecting. Any non-EOF data
    // must now close the connection outright.
    use tokio::io::AsyncWriteExt;

    let (addr, _bus, metrics, _shutdown_tx) = spawn_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    // Past PEEK_TIMEOUT (500ms) so this is classified as plain TCP, not
    // still mid-WS-detection when the disconnect happens.
    tokio::time::sleep(Duration::from_millis(600)).await;
    stream
        .write_all(b"unexpected client data\r\n")
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if metrics
                .render_prometheus_text()
                .contains("manta_json_clients_connected 0")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("client that sent data was never disconnected");
}

#[tokio::test]
async fn a_websocket_handshake_split_across_tcp_writes_is_still_detected() {
    // Regression test: the classifying peek must not misjudge a genuine
    // WebSocket client as plain TCP just because its "GET" arrived in
    // pieces smaller than 3 bytes on the first look.
    use tokio::io::AsyncWriteExt;

    let (addr, _bus, metrics, _shutdown_tx) = spawn_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    stream.write_all(b"GE").await.unwrap(); // fewer than 3 bytes
    tokio::time::sleep(Duration::from_millis(50)).await; // let the first peek see only "GE"
    stream
        .write_all(
            b"T / HTTP/1.1\r\n\
              Host: localhost\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if metrics
                .render_prometheus_text()
                .contains("manta_ws_clients_connected 1")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("split GET was never classified as a WebSocket client");
}

#[tokio::test]
async fn a_websocket_handshake_arriving_slower_than_the_old_peek_timeout_is_still_detected() {
    // Regression test (round-14 review): the classifying peek's own
    // budget (PEEK_TIMEOUT, 500ms) used to be the ONLY patience a
    // slow-arriving handshake got, even though the separately declared
    // HANDSHAKE_TIMEOUT is 10s. A genuine WS client whose remaining bytes
    // arrive after 500ms but well within 10s must still be classified as
    // WebSocket, not misclassified as raw TCP (which then closes it once
    // the rest of the HTTP request shows up as "unexpected client data").
    use tokio::io::AsyncWriteExt;

    let (addr, _bus, metrics, _shutdown_tx) = spawn_server().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    stream.write_all(b"GE").await.unwrap(); // fewer than 3 bytes
                                            // Past the OLD 500ms PEEK_TIMEOUT, comfortably under the 10s
                                            // HANDSHAKE_TIMEOUT.
    tokio::time::sleep(Duration::from_millis(800)).await;
    stream
        .write_all(
            b"T / HTTP/1.1\r\n\
              Host: localhost\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if metrics
                .render_prometheus_text()
                .contains("manta_ws_clients_connected 1")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("slow-arriving GET was never classified as a WebSocket client");
}

#[tokio::test]
async fn tcp_and_websocket_clients_share_the_same_port() {
    // ARCHITECTURE §7 documents one shared "tcp/ws :7301" port -- prove a
    // raw TCP client and a WebSocket client can both connect to the exact
    // same listener and each get correctly classified.
    let (addr, bus, _metrics, _shutdown_tx) = spawn_server().await;

    let tcp_stream = TcpStream::connect(addr).await.unwrap();
    let mut tcp_reader = BufReader::new(tcp_stream);

    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect failed");

    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(sample_spot());

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), tcp_reader.read_line(&mut line))
        .await
        .expect("timed out waiting for TCP JSON line")
        .unwrap();
    let tcp_value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
    assert_eq!(tcp_value["dxCall"], "JA1ABC");

    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for ws message")
        .expect("stream ended")
        .expect("ws error");
    let ws_value: serde_json::Value =
        serde_json::from_str(&msg.into_text().unwrap()).expect("valid JSON message");
    assert_eq!(ws_value["dxCall"], "JA1ABC");

    let _ = ws.close(None).await;
}

#[tokio::test]
async fn websocket_client_receives_spot_as_json_message() {
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    let cty = Arc::new(Table::parse(CTY_FIXTURE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let bus2 = bus.clone();
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus: bus2,
                metrics,
                cty,
                station_call: STATION_CALL.to_string(),
                decoder_version: "manta-test".to_string(),
                shutdown: shutdown_rx,
            },
            tasks,
            limiter,
        )
        .await;
    });

    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect failed");

    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(sample_spot());

    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for ws message")
        .expect("stream ended")
        .expect("ws error");

    let text = msg.into_text().expect("expected a text frame");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON message");
    assert_eq!(value["source"], "skimmer");
    assert_eq!(value["dxCall"], "JA1ABC");

    let _ = ws.close(None).await;
}

#[tokio::test]
async fn websocket_client_sending_an_oversized_message_is_disconnected() {
    // Regression test (round-5 review): the WS handshake used to accept
    // tungstenite's default 64 MiB max-message-size, letting a client
    // force a large per-connection buffer allocation before a frame is
    // even inspected -- a memory-exhaustion DoS multiplied across
    // connections. `handle_ws_client` now configures a small explicit
    // limit; a client that exceeds it must be disconnected, not served.
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    let cty = Arc::new(Table::parse(CTY_FIXTURE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus,
                metrics,
                cty,
                station_call: STATION_CALL.to_string(),
                decoder_version: "manta-test".to_string(),
                shutdown: shutdown_rx,
            },
            tasks,
            limiter,
        )
        .await;
    });

    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect failed");

    // Comfortably past MAX_INBOUND_WS_MESSAGE_BYTES (16 KiB).
    let oversized = "a".repeat(64 * 1024);
    let _ = ws.send(Message::Text(oversized.into())).await;

    let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("server never responded to the oversized message");
    match outcome {
        None => {}                        // connection closed
        Some(Err(_)) => {}                // protocol error surfaced to the client
        Some(Ok(Message::Close(_))) => {} // clean close frame
        other => panic!("expected the oversized message to end the connection, got {other:?}"),
    }
}

#[tokio::test]
async fn websocket_client_sending_an_unsolicited_pong_is_disconnected() {
    // Regression test (round-7 review): this server never sends Ping, so
    // any inbound Pong is unsolicited. The round-6 fix disconnected on
    // Text/Binary application data but still silently ignored Pong
    // frames -- a client flooding valid small Pong frames kept the read
    // arm perpetually ready, recreating the exact CPU-exhaustion loop the
    // Text/Binary rejection was meant to close off.
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    let cty = Arc::new(Table::parse(CTY_FIXTURE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus,
                metrics,
                cty,
                station_call: STATION_CALL.to_string(),
                decoder_version: "manta-test".to_string(),
                shutdown: shutdown_rx,
            },
            tasks,
            limiter,
        )
        .await;
    });

    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect failed");

    let _ = ws.send(Message::Pong(vec![].into())).await;

    let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("server never responded to the unsolicited pong");
    match outcome {
        None => {}                        // connection closed
        Some(Err(_)) => {}                // protocol error surfaced to the client
        Some(Ok(Message::Close(_))) => {} // clean close frame
        other => panic!("expected the unsolicited pong to end the connection, got {other:?}"),
    }
}

#[tokio::test]
async fn websocket_client_flooding_pings_past_the_budget_is_disconnected() {
    // Regression test (round-13 review): unlike Pong/Text/Binary, a Ping
    // frame is legitimate client behavior -- so it can't just be rejected
    // outright the way those are. But replying to an UNLIMITED sequence of
    // them still keeps the read arm perpetually ready, recreating the
    // same CPU/bandwidth-exhaustion shape. A client must be disconnected
    // once it exceeds a small lifetime Ping budget, generous enough for
    // any real keepalive cadence.
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, SystemTime::now(), 0));
    let metrics = Arc::new(Metrics::new());
    let cty = Arc::new(Table::parse(CTY_FIXTURE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::json_stream::serve(
            listener,
            manta_server::json_stream::JsonStreamConfig {
                bus,
                metrics,
                cty,
                station_call: STATION_CALL.to_string(),
                decoder_version: "manta-test".to_string(),
                shutdown: shutdown_rx,
            },
            tasks,
            limiter,
        )
        .await;
    });

    let url = format!("ws://{addr}");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect failed");

    for _ in 0..manta_server::json_stream::MAX_INBOUND_PINGS {
        ws.send(Message::Ping(vec![].into())).await.unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a Pong within the budget")
            .expect("stream ended before the budget was exhausted")
            .expect("ws error before the budget was exhausted");
        assert!(
            matches!(pong, Message::Pong(_)),
            "expected a Pong reply within budget, got {pong:?}"
        );
    }

    // One more Ping, past the budget, must end the connection instead of
    // getting another Pong.
    let _ = ws.send(Message::Ping(vec![].into())).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("server never responded after the ping budget was exceeded");
    match outcome {
        None => {}                        // connection closed
        Some(Err(_)) => {}                // protocol error surfaced to the client
        Some(Ok(Message::Close(_))) => {} // clean close frame
        other => panic!("expected exceeding the ping budget to end the connection, got {other:?}"),
    }
}
