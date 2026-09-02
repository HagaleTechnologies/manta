//! JSON Lines spot stream, over plain TCP and WebSocket, sharing one port.
//! ARCHITECTURE §7: "JSON Lines stream (TCP and WebSocket, :7301)... this
//! is the cqdx ingest surface." A WebSocket client is distinguished from a
//! raw JSON Lines client by peeking the connection's first bytes for an
//! HTTP `GET` upgrade request before either side has sent anything --
//! a raw JSON Lines client sends nothing at all (this protocol is pure
//! server push), so a peek timeout with no bytes is itself the "not a
//! WebSocket handshake" signal, not an error.

use crate::bus::{BusSpot, SpotBus};
use crate::metrics::Metrics;
use crate::spot_message::SpotMessage;
use manta_spot::cty::Table;
use manta_spot::Spot;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ErrorKind};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// How long a slow client is given to accept one write before it's treated
/// as stalled and disconnected -- otherwise a write blocked forever would
/// keep the handler from ever returning to `rx.recv()` to observe a
/// `Lagged` error, defeating the "slow clients are disconnected, never
/// back-pressured" policy (ARCHITECTURE §7).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a WebSocket client has to complete its opening handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `serve` waits for a connection's first bytes before assuming
/// it's a raw JSON Lines client (which may never send anything).
const PEEK_TIMEOUT: Duration = Duration::from_millis(500);

fn render(
    spot: &Spot,
    station_call: &str,
    cty: &Table,
    decoder_version: &str,
    unix_ts: i64,
    session_epoch_unix: i64,
) -> String {
    let msg = SpotMessage::from_spot(
        spot,
        station_call,
        cty,
        decoder_version,
        unix_ts,
        session_epoch_unix,
    );
    serde_json::to_string(&msg).expect("SpotMessage always serializes")
}

/// Accepts connections on `listener`, dispatching each to the plain-TCP or
/// WebSocket handler based on a non-destructive peek of its first bytes.
pub async fn serve(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let bus = bus.clone();
        let metrics = metrics.clone();
        let cty = cty.clone();
        let station_call = station_call.clone();
        let decoder_version = decoder_version.clone();
        tokio::spawn(async move {
            // Subscribe immediately on accept, before the WS-detection
            // peek (which can wait up to PEEK_TIMEOUT) -- otherwise a spot
            // published while a connection is still being classified is
            // lost to the broadcast channel's no-history-for-late-
            // subscribers semantics (same class of bug the telnet server
            // hit with subscribe-after-login).
            let rx = bus.subscribe();
            if looks_like_websocket_handshake(&socket).await {
                metrics.inc_ws_clients();
                let _ = handle_ws_client(
                    socket,
                    rx,
                    bus,
                    metrics.clone(),
                    cty,
                    station_call,
                    decoder_version,
                )
                .await;
                metrics.dec_ws_clients();
            } else {
                metrics.inc_json_clients();
                let _ = handle_tcp_client(
                    socket,
                    rx,
                    bus,
                    metrics.clone(),
                    cty,
                    station_call,
                    decoder_version,
                )
                .await;
                metrics.dec_json_clients();
            }
        });
    }
}

async fn looks_like_websocket_handshake(socket: &TcpStream) -> bool {
    let mut peek_buf = [0u8; 3];
    matches!(
        tokio::time::timeout(PEEK_TIMEOUT, socket.peek(&mut peek_buf)).await,
        Ok(Ok(n)) if n >= 3 && &peek_buf[..3] == b"GET"
    )
}

async fn handle_tcp_client(
    mut socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) -> std::io::Result<()> {
    // This protocol is pure server push -- the client never needs to send
    // anything -- so this scratch buffer only exists to notice EOF/close;
    // any bytes a client does send are unexpected and simply discarded.
    let mut scratch = [0u8; 64];
    loop {
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        let unix_ts = bus.unix_ts_for(bus_spot.spot.sample_ts);
                        let line = render(
                            &bus_spot.spot,
                            &station_call,
                            &cty,
                            &decoder_version,
                            unix_ts,
                            bus.epoch_unix_secs(),
                        );
                        tokio::time::timeout(WRITE_TIMEOUT, async {
                            socket.write_all(line.as_bytes()).await?;
                            socket.write_all(b"\n").await
                        })
                        .await
                        .map_err(|_| std::io::Error::new(ErrorKind::TimedOut, "write timed out"))??;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        metrics.record_lagged(n);
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            // Detects the client closing the connection during a quiet
            // period -- without this, only `rx.recv()` is ever polled, so
            // a closed-but-idle client's task/socket/gauge would linger
            // until the next spot's write happened to fail.
            read_result = socket.read(&mut scratch) => {
                match read_result {
                    Ok(0) => return Ok(()),  // client closed the connection
                    Ok(_) => {}              // unexpected client data; ignore
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

async fn handle_ws_client(
    socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut ws =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::accept_async(socket)).await??;
    loop {
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        let unix_ts = bus.unix_ts_for(bus_spot.spot.sample_ts);
                        let text = render(
                            &bus_spot.spot,
                            &station_call,
                            &cty,
                            &decoder_version,
                            unix_ts,
                            bus.epoch_unix_secs(),
                        );
                        tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Text(text.into())))
                            .await
                            .map_err(|_| anyhow::anyhow!("write timed out"))??;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        metrics.record_lagged(n);
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(Message::Ping(payload))) => {
                        tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Pong(payload)))
                            .await
                            .map_err(|_| anyhow::anyhow!("write timed out"))??;
                    }
                    Some(Ok(_)) => {} // client isn't expected to send data; ignore other frames
                    Some(Err(_)) => return Ok(()),
                }
            }
        }
    }
}
