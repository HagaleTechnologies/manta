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
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ErrorKind};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};

/// How long a slow client is given to accept one write before it's treated
/// as stalled and disconnected -- otherwise a write blocked forever would
/// keep the handler from ever returning to `rx.recv()` to observe a
/// `Lagged` error, defeating the "slow clients are disconnected, never
/// back-pressured" policy (ARCHITECTURE §7).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a WebSocket client has to complete its opening handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Caps inbound WebSocket message/frame size. This stream is pure server
/// push -- a client is only ever expected to send tiny control frames
/// (Ping/Close, see `handle_ws_client`) -- so tungstenite's 64 MiB/16 MiB
/// defaults are far larger than legitimate traffic ever needs and would
/// let a client force a per-connection buffer allocation up to that
/// default before the frame is even inspected (round-5 review finding: a
/// memory-exhaustion DoS multiplied across connections). 16 KiB is
/// generous headroom over any real control frame.
const MAX_INBOUND_WS_MESSAGE_BYTES: usize = 16 * 1024;
/// How long `serve` waits for a connection's first bytes before assuming
/// it's a raw JSON Lines client (which may never send anything).
const PEEK_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the accept loop backs off after a failed `accept()` before
/// retrying -- a persistent resource error (e.g. the process's
/// file-descriptor limit, `EMFILE`) makes `accept()` return immediately
/// with an error, and retrying with no delay turns this into a tight loop
/// that starves other tasks on the same runtime instead of giving the
/// system a chance to recover.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Everything a per-connection handler needs besides the socket and its
/// broadcast subscription -- grouped so `handle_tcp_client`/
/// `handle_ws_client` stay at a sane arity as this crate's shared-context
/// list grows.
#[derive(Clone)]
struct ClientCtx {
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
    shutdown: watch::Receiver<bool>,
}

impl ClientCtx {
    fn render(&self, bus_spot: &BusSpot) -> String {
        let unix_ts = self.bus.unix_ts_for(bus_spot.spot.sample_ts);
        let msg = SpotMessage::from_spot(
            &bus_spot.spot,
            &self.station_call,
            &self.cty,
            &self.decoder_version,
            unix_ts,
            self.bus.session_nonce(),
        );
        serde_json::to_string(&msg).expect("SpotMessage always serializes")
    }
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
    shutdown: watch::Receiver<bool>,
) {
    let ctx = ClientCtx {
        bus,
        metrics,
        cty,
        station_call,
        decoder_version,
        shutdown,
    };
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => {
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            // Subscribe immediately on accept, before the WS-detection
            // peek (which can wait up to PEEK_TIMEOUT) -- otherwise a spot
            // published while a connection is still being classified is
            // lost to the broadcast channel's no-history-for-late-
            // subscribers semantics (same class of bug the telnet server
            // hit with subscribe-after-login).
            let rx = ctx.bus.subscribe();
            if looks_like_websocket_handshake(&socket).await {
                ctx.metrics.inc_ws_clients();
                let _ = handle_ws_client(socket, rx, ctx.clone()).await;
                ctx.metrics.dec_ws_clients();
            } else {
                ctx.metrics.inc_json_clients();
                let _ = handle_tcp_client(socket, rx, ctx.clone()).await;
                ctx.metrics.dec_json_clients();
            }
        });
    }
}

/// How long to wait between re-peeks while fewer than 3 bytes have arrived
/// so far -- a single peek only ever returns what's *currently* in the
/// kernel receive buffer, so a `GET` split across TCP segments (e.g. `G`
/// then `ET ...`) must not be misclassified as "not a WebSocket client"
/// just because the first peek alone came up short.
const PEEK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

async fn looks_like_websocket_handshake(socket: &TcpStream) -> bool {
    let deadline = tokio::time::Instant::now() + PEEK_TIMEOUT;
    let mut peek_buf = [0u8; 3];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false; // budget exhausted with < 3 bytes ever seen
        }
        match tokio::time::timeout(remaining, socket.peek(&mut peek_buf)).await {
            Ok(Ok(n)) if n >= 3 => return &peek_buf[..3] == b"GET",
            Ok(Ok(0)) => return false, // peer closed without sending anything
            Ok(Ok(_)) => tokio::time::sleep(PEEK_RETRY_INTERVAL).await,
            Ok(Err(_)) | Err(_) => return false,
        }
    }
}

async fn handle_tcp_client(
    mut socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
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
                        let line = ctx.render(&bus_spot);
                        tokio::time::timeout(WRITE_TIMEOUT, async {
                            socket.write_all(line.as_bytes()).await?;
                            socket.write_all(b"\n").await
                        })
                        .await
                        .map_err(|_| std::io::Error::new(ErrorKind::TimedOut, "write timed out"))??;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        ctx.metrics.record_lagged(n);
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
                    Ok(0) => return Ok(()), // client closed the connection
                    // Any non-EOF data is a protocol violation on this
                    // pure-server-push stream -- close instead of looping
                    // back to read again. Looping (the prior behavior)
                    // made this branch perpetually ready under a client
                    // that keeps sending data, starving the spot-write
                    // branch and burning CPU in a tight select! loop
                    // (round-5 review finding).
                    Ok(_) => return Ok(()),
                    Err(_) => return Ok(()),
                }
            }
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                while let Ok(bus_spot) = rx.try_recv() {
                    let line = ctx.render(&bus_spot);
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, async {
                        socket.write_all(line.as_bytes()).await?;
                        socket.write_all(b"\n").await
                    })
                    .await;
                }
                return Ok(());
            }
        }
    }
}

async fn handle_ws_client(
    socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_INBOUND_WS_MESSAGE_BYTES));
    let mut ws = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(socket, Some(ws_config)),
    )
    .await??;
    loop {
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        let text = ctx.render(&bus_spot);
                        tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Text(text.into())))
                            .await
                            .map_err(|_| anyhow::anyhow!("write timed out"))??;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        ctx.metrics.record_lagged(n);
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
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                while let Ok(bus_spot) = rx.try_recv() {
                    let text = ctx.render(&bus_spot);
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Text(text.into()))).await;
                }
                return Ok(());
            }
        }
    }
}
