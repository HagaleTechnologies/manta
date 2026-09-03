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
use crate::rate_limit::IpRateLimiter;
use crate::spot_message::SpotMessage;
use crate::tasks::{ClientTasks, ConnectionLimiter, IpQuota};
use manta_spot::cty::Table;
use std::net::IpAddr;
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
/// Ping rate budget: at most this many Ping frames per `PING_RATE_WINDOW`.
/// Unlike Pong/Text/Binary (never legitimate on this pure-server-push
/// stream, so rejected outright), Ping is genuine client behavior this
/// server must answer -- but replying to an UNLIMITED sequence of them
/// keeps the read arm perpetually ready, recreating the same
/// CPU/bandwidth-exhaustion shape (round-13 review finding). A per-window
/// RATE, not a lifetime total -- a lifetime cap disconnects a
/// well-behaved long-running client just for staying connected a long
/// time (one Ping/minute hits a 60-count lifetime cap after an hour
/// regardless of pacing; round-14 review finding). 10 pings per minute is
/// generous headroom over any real keepalive cadence.
pub const MAX_INBOUND_PINGS: u32 = 10;
pub const PING_RATE_WINDOW: Duration = Duration::from_secs(60);
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
/// Upper bound on concurrently admitted JSON-Lines/WebSocket clients (this
/// listener serves both over one port). With no cap at all, an
/// unauthenticated client could open connections without bound, each one
/// costing a socket, a tracked task, and its own broadcast subscription
/// that every future publish must additionally fan out to (round-15 review
/// finding). Generous headroom over any realistic legitimate consumer
/// count (this is the cqdx ingest surface, not a public high-fanout feed).
pub const MAX_JSON_STREAM_CONNECTIONS: usize = 512;
/// Upper bound on concurrently admitted JSON/WS clients from a SINGLE
/// source IP (MAN-61, `docs/DECISIONS/2026-09-03-man61-per-ip-connection-
/// quota.md`): a raw JSON client is *designed* to be quiet forever after
/// connecting (that's the whole point of a push-only protocol), so
/// `MAX_JSON_STREAM_CONNECTIONS` alone lets one source open up to that
/// many connections, send nothing further, and permanently deny
/// admission to every other client. Same reasoning and value as
/// `telnet::MAX_TELNET_CONNECTIONS_PER_IP`.
pub const MAX_JSON_STREAM_CONNECTIONS_PER_IP: usize = 16;

// MAN-57: `ping_limiter` inside `handle_ws_client` is per-CONNECTION, so a
// source opening several connections (up to
// `MAX_JSON_STREAM_CONNECTIONS_PER_IP`) gets that many independent full
// Ping budgets. `serve`'s `ip_ping_limiter` parameter below is a shared
// sibling checked in addition to each connection's own budget, using the
// same `MAX_INBOUND_PINGS`/`PING_RATE_WINDOW` values -- the intent was
// always "this many Pings per source", not "per connection". Same fix and
// reasoning as `telnet::serve`'s `ip_command_limiter`.

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

/// Everything `serve` needs to start accepting JSON/WebSocket clients,
/// besides the listener itself and the shared `ClientTasks` registry --
/// grouped to keep `serve`'s arity sane as this crate's shared-context
/// list grows (the same reasoning that produced the internal `ClientCtx`
/// this gets unpacked into).
pub struct JsonStreamConfig {
    pub bus: Arc<SpotBus>,
    pub metrics: Arc<Metrics>,
    pub cty: Arc<Table>,
    pub station_call: String,
    pub decoder_version: String,
    pub shutdown: watch::Receiver<bool>,
}

/// Accepts connections on `listener`, dispatching each to the plain-TCP or
/// WebSocket handler based on a non-destructive peek of its first bytes.
pub async fn serve(
    listener: TcpListener,
    config: JsonStreamConfig,
    tasks: ClientTasks,
    limiter: ConnectionLimiter,
    ip_quota: IpQuota,
    ip_ping_limiter: IpRateLimiter,
) {
    let ctx = ClientCtx {
        bus: config.bus,
        metrics: config.metrics,
        cty: config.cty,
        station_call: config.station_call,
        decoder_version: config.decoder_version,
        shutdown: config.shutdown,
    };
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => {
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        // Checked BEFORE the shared limiter, not after (MAN-61): a source
        // already at its own per-IP cap is declined without consuming a
        // `ConnectionLimiter` permit at all -- the socket is simply
        // dropped here, closing the connection, leaving that shared
        // capacity for other sources.
        let Some(ip_guard) = ip_quota.try_acquire(peer.ip()) else {
            tracing::warn!(ip = %peer.ip(), "json_stream: per-IP connection quota exceeded, declining");
            continue;
        };
        // Blocks the accept loop itself (not just the client) until
        // capacity is available -- a flood beyond
        // `MAX_JSON_STREAM_CONNECTIONS` is left waiting in the OS's own
        // connection backlog rather than ever being admitted, tracked, or
        // given a broadcast subscription at all (round-15 review finding).
        let Ok(permit) = limiter.clone().acquire_owned().await else {
            continue; // limiter closed: unreachable in practice, never panics
        };
        // Subscribe immediately on accept, before `tokio::spawn` -- not
        // just before the WS-detection peek inside the spawned task, which
        // still leaves a real gap: `tokio::spawn` only schedules the task,
        // it doesn't guarantee the task is polled before this loop moves
        // on. A spot published after `accept()` succeeds but before the
        // spawned task is first polled would otherwise be lost to the
        // broadcast channel's no-history-for-late-subscribers semantics on
        // a busy runtime or a high-rate stream (round-7 review finding;
        // same fix applied to the telnet accept path below).
        let rx = ctx.bus.subscribe();
        let ctx = ctx.clone();
        let peer_ip = peer.ip();
        let ip_ping_limiter = ip_ping_limiter.clone();
        // Tracked in the shared `ClientTasks` registry (not a bare
        // `tokio::spawn`) so a shutdown sequence can genuinely AWAIT this
        // task's completion instead of guessing a fixed grace period
        // (round-10 review finding).
        tasks.lock().await.spawn(async move {
            let _permit = permit; // held for the connection's lifetime
            let _ip_guard = ip_guard; // held for the connection's lifetime
            if looks_like_websocket_handshake(&socket).await {
                ctx.metrics.inc_ws_clients();
                let _ =
                    handle_ws_client(socket, rx, ctx.clone(), peer, peer_ip, ip_ping_limiter).await;
                ctx.metrics.dec_ws_clients();
            } else {
                ctx.metrics.inc_json_clients();
                let _ = handle_tcp_client(socket, rx, ctx.clone(), peer).await;
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
    let mut deadline = tokio::time::Instant::now() + PEEK_TIMEOUT;
    let mut peek_buf = [0u8; 3];
    let mut seen_any_bytes = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false; // budget exhausted with < 3 bytes ever seen
        }
        match tokio::time::timeout(remaining, socket.peek(&mut peek_buf)).await {
            Ok(Ok(n)) if n >= 3 => return &peek_buf[..3] == b"GET",
            Ok(Ok(0)) => return false, // peer closed without sending anything
            Ok(Ok(_)) => {
                if !seen_any_bytes {
                    // Some evidence of an in-progress handshake (e.g. "G"
                    // arrived but "ET" hasn't yet) -- extend patience to
                    // the FULL handshake budget, not just the short
                    // no-bytes-yet PEEK_TIMEOUT, so a slow-arriving but
                    // genuine WS client isn't misclassified as raw JSON
                    // and handed to the raw-TCP handler, which then closes
                    // it once the rest of the HTTP request arrives as
                    // "unexpected client data" (round-14 review finding).
                    // A client that has sent NOTHING yet still only gets
                    // the original short PEEK_TIMEOUT, so a genuine raw
                    // JSON client (expected to send nothing at all) is
                    // still classified quickly, not delayed to the full
                    // handshake budget for no reason.
                    seen_any_bytes = true;
                    deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
                }
                tokio::time::sleep(PEEK_RETRY_INTERVAL).await;
            }
            Ok(Err(_)) | Err(_) => return false,
        }
    }
}

#[tracing::instrument(name = "json_client", skip(socket, rx, ctx), fields(peer = %peer))]
async fn handle_tcp_client(
    mut socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
    peer: std::net::SocketAddr,
) -> std::io::Result<()> {
    tracing::info!("json_stream: raw TCP client connected");
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
                        let write_result = tokio::time::timeout(WRITE_TIMEOUT, async {
                            socket.write_all(line.as_bytes()).await?;
                            socket.write_all(b"\n").await
                        })
                        .await
                        .unwrap_or_else(|_| Err(std::io::Error::new(ErrorKind::TimedOut, "write timed out")));
                        if write_result.is_err() {
                            // The write for THIS spot failed, plus
                            // whatever's still retained in `rx` is
                            // abandoned along with it -- a bare `?` here
                            // (the prior behavior) exited with neither
                            // counted anywhere (round-11 review finding).
                            ctx.metrics.record_write_failed(1 + rx.len() as u64);
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // `n` alone under-counts: this receiver is about
                        // to be disconnected (never drained further), so
                        // whatever it still has retained is lost too, not
                        // just what the channel already evicted (round-9
                        // review finding).
                        let lost = crate::bus::total_lag_loss(n, &rx);
                        tracing::warn!(lost, "json_stream: client lagged behind broadcast, disconnecting");
                        ctx.metrics.record_lagged(lost);
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
                    Ok(0) => {
                        tracing::info!("json_stream: raw TCP client disconnected");
                        return Ok(());
                    }
                    // Any non-EOF data is a protocol violation on this
                    // pure-server-push stream -- close instead of looping
                    // back to read again. Looping (the prior behavior)
                    // made this branch perpetually ready under a client
                    // that keeps sending data, starving the spot-write
                    // branch and burning CPU in a tight select! loop
                    // (round-5 review finding).
                    Ok(_) => {
                        tracing::warn!("json_stream: unexpected client data on pure-push stream, disconnecting");
                        return Ok(());
                    }
                    Err(_) => return Ok(()),
                }
            }
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                // A `Lagged(n)` mid-drain means this subscriber missed `n`
                // spots, not that the channel is empty -- there can still
                // be spots queued after the gap. Stopping on the first
                // `Err` (the prior behavior) silently dropped everything
                // from that point on without even recording the loss
                // (round-6 review finding).
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            let line = ctx.render(&bus_spot);
                            let write_result = tokio::time::timeout(WRITE_TIMEOUT, async {
                                socket.write_all(line.as_bytes()).await?;
                                socket.write_all(b"\n").await
                            })
                            .await;
                            if !matches!(write_result, Ok(Ok(()))) {
                                // The client's socket is presumably dead --
                                // further writes would just fail too, so
                                // stop draining and count what's abandoned
                                // (this failed spot plus anything still
                                // retained), rather than silently
                                // discarding the error and continuing to
                                // burn the write timeout on every remaining
                                // queued spot (round-12 review finding).
                                ctx.metrics.record_write_failed(1 + rx.len() as u64);
                                return Ok(());
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            ctx.metrics.record_lagged(n);
                        }
                        Err(_) => break, // Empty or Closed: nothing left to drain
                    }
                }
                return Ok(());
            }
        }
    }
}

#[tracing::instrument(
    name = "ws_client",
    skip(socket, rx, ctx, peer_ip, ip_ping_limiter),
    fields(peer = %peer)
)]
async fn handle_ws_client(
    socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
    peer: std::net::SocketAddr,
    peer_ip: IpAddr,
    ip_ping_limiter: IpRateLimiter,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_INBOUND_WS_MESSAGE_BYTES));
    let ws_result = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(socket, Some(ws_config)),
    )
    .await;
    let mut ws = match ws_result {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "json_stream: WS handshake rejected");
            return Err(e.into());
        }
        Err(_) => {
            tracing::warn!("json_stream: WS handshake timed out");
            return Err(anyhow::anyhow!("WS handshake timed out"));
        }
    };
    tracing::info!("json_stream: WS client connected");
    let mut ping_limiter = crate::rate_limit::RateLimiter::new(MAX_INBOUND_PINGS, PING_RATE_WINDOW);
    loop {
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        let text = ctx.render(&bus_spot);
                        let write_result =
                            tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Text(text.into())))
                                .await;
                        let write_failed = !matches!(write_result, Ok(Ok(())));
                        if write_failed {
                            // The write for THIS spot failed, plus
                            // whatever's still retained in `rx` is
                            // abandoned along with it -- a bare `?` here
                            // (the prior behavior) exited with neither
                            // counted anywhere (round-11 review finding).
                            ctx.metrics.record_write_failed(1 + rx.len() as u64);
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // See the TCP handler's identical Lagged branch
                        // above for why `n` alone under-counts.
                        let lost = crate::bus::total_lag_loss(n, &rx);
                        tracing::warn!(lost, "json_stream: WS client lagged behind broadcast, disconnecting");
                        ctx.metrics.record_lagged(lost);
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("json_stream: WS client disconnected");
                        return Ok(());
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        // A Ping IS legitimate client behavior, unlike
                        // Pong/Text/Binary below -- but replying to an
                        // UNLIMITED sequence of them keeps this arm
                        // perpetually ready the same way those did
                        // (round-13 review finding). A RATE budget, not a
                        // lifetime total -- a lifetime cap would disconnect
                        // a well-behaved long-running client just for
                        // staying connected a long time (round-14 review
                        // finding).
                        // Checked in addition to (never instead of) the
                        // per-connection budget above -- MAN-57: without
                        // this, a source opening several connections gets
                        // an independent full Ping budget on each one,
                        // multiplying the intended per-source rate by
                        // however many connections it holds.
                        if !ping_limiter.allow() || !ip_ping_limiter.allow(peer_ip) {
                            tracing::warn!("json_stream: client exceeded Ping rate budget, disconnecting");
                            return Ok(());
                        }
                        tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Pong(payload)))
                            .await
                            .map_err(|_| anyhow::anyhow!("write timed out"))??;
                    }
                    // This server never sends Ping, so ANY inbound Pong is
                    // unsolicited -- treat it the same as Text/Binary
                    // below, not as harmless. Silently ignoring it (the
                    // prior behavior) let a client flood valid small Pong
                    // frames and kept this arm perpetually ready, recreating
                    // the exact CPU-exhaustion loop the Text/Binary
                    // rejection was meant to close off (round-7 review
                    // finding).
                    Some(Ok(Message::Pong(_))) => {
                        tracing::warn!("json_stream: unsolicited Pong on pure-push stream, disconnecting");
                        return Ok(());
                    }
                    // Text/Binary/raw Frame: this stream is pure server
                    // push, so any application-data frame is a protocol
                    // violation -- disconnect rather than ignore. Bounding
                    // each message's SIZE (MAX_INBOUND_WS_MESSAGE_BYTES)
                    // isn't enough on its own: a client sending an
                    // unbounded SEQUENCE of small messages kept this arm
                    // perpetually ready, burning CPU indefinitely (round-6
                    // review finding).
                    Some(Ok(_)) => {
                        tracing::warn!("json_stream: unexpected data frame on pure-push stream, disconnecting");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "json_stream: malformed WS frame, disconnecting");
                        return Ok(());
                    }
                }
            }
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                // See the TCP handler's identical shutdown-drain branch
                // above for why `Lagged` must not stop the drain.
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            let text = ctx.render(&bus_spot);
                            let write_result = tokio::time::timeout(
                                WRITE_TIMEOUT,
                                ws.send(Message::Text(text.into())),
                            )
                            .await;
                            if !matches!(write_result, Ok(Ok(()))) {
                                // See the TCP handler's identical
                                // shutdown-drain branch for why a failed
                                // write stops the drain instead of
                                // silently continuing (round-12 review
                                // finding).
                                ctx.metrics.record_write_failed(1 + rx.len() as u64);
                                return Ok(());
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            ctx.metrics.record_lagged(n);
                        }
                        Err(_) => break,
                    }
                }
                return Ok(());
            }
        }
    }
}
