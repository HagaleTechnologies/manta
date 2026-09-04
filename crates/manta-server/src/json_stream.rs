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

/// MAN-68 (PR #85 review round 6): `handle_ws_client`'s WS handshake
/// rejection/timeout branches already log a specific warning before
/// returning `Err` -- `serve`'s task-boundary catch-all then logged a
/// SECOND, generic warning for the same `Err`. Same fix and reasoning as
/// `telnet::ClientError`: `Logged` for an error a specific branch already
/// reported (the catch-all skips it), `Unlogged` for anything else, so a
/// future fallible site that doesn't opt into `Logged` still reaches the
/// catch-all rather than silently going unrecorded.
enum WsClientError {
    Logged,
    Unlogged(anyhow::Error),
}

impl From<anyhow::Error> for WsClientError {
    fn from(e: anyhow::Error) -> Self {
        WsClientError::Unlogged(e)
    }
}

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
    drain_deadline: Duration,
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
    /// MAN-45 (round-16 finding): the per-client shutdown-drain deadline --
    /// see `tasks::CLIENT_DRAIN_DEADLINE`'s doc comment for why this must
    /// live on each handler's own loop rather than only the outer
    /// registry-wide `await_all` deadline.
    pub drain_deadline: Duration,
}

/// MAN-59 review round 2: same rationale as `telnet::QUOTA_REJECT_LOG_MAX_PER_WINDOW`
/// -- this warning runs on every rejected socket, before any request rate
/// limiter, so a source completing repeated handshakes despite holding
/// its allotment could otherwise flood the log sink unbounded.
const QUOTA_REJECT_LOG_MAX_PER_WINDOW: u32 = 1;
const QUOTA_REJECT_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-59 review round 4: see `telnet::CONNECTION_LOG_MAX_PER_WINDOW`'s
/// doc comment for the full rationale -- rounds 2-3 each found one more
/// individually un-gated log call site in this file too, the same
/// recurring shape the policy's "reconsider the fix strategy" signal
/// covers. ONE budget decided once per admitted connection (`serve`
/// below), threaded through as `log_enabled`, gates every tracing call
/// for that connection's lifetime in both `handle_tcp_client` and
/// `handle_ws_client`.
const CONNECTION_LOG_MAX_PER_WINDOW: u32 = 30;
const CONNECTION_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-68 (PR #85 review round 7): see `telnet::REJECTION_LOG_MAX_PER_WINDOW`'s
/// doc comment for the full rationale -- a separate, per-IP budget for
/// genuinely security-relevant rejections (WS Ping-rate-budget-exceeded,
/// a malformed WS frame), checked independently of `log_enabled` so
/// ordinary connection churn exhausting the shared lifecycle log budget
/// can never suppress one of these.
const REJECTION_LOG_MAX_PER_WINDOW: u32 = 30;
const REJECTION_LOG_WINDOW: Duration = Duration::from_secs(60);

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
        drain_deadline: config.drain_deadline,
    };
    let quota_reject_log_limiter =
        IpRateLimiter::new(QUOTA_REJECT_LOG_MAX_PER_WINDOW, QUOTA_REJECT_LOG_WINDOW);
    crate::rate_limit::spawn_stale_entry_reaper(quota_reject_log_limiter.clone());
    let connection_log_limiter =
        IpRateLimiter::new(CONNECTION_LOG_MAX_PER_WINDOW, CONNECTION_LOG_WINDOW);
    crate::rate_limit::spawn_stale_entry_reaper(connection_log_limiter.clone());
    let rejection_log_limiter =
        IpRateLimiter::new(REJECTION_LOG_MAX_PER_WINDOW, REJECTION_LOG_WINDOW);
    crate::rate_limit::spawn_stale_entry_reaper(rejection_log_limiter.clone());
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
            if quota_reject_log_limiter.allow(peer.ip()) {
                tracing::warn!(ip = %peer.ip(), "json_stream: per-IP connection quota exceeded, declining");
            }
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
        let rejection_log_limiter = rejection_log_limiter.clone();
        // Decided ONCE per admitted connection -- see
        // CONNECTION_LOG_MAX_PER_WINDOW's doc comment above.
        let log_enabled = connection_log_limiter.allow(peer_ip);
        // Tracked in the shared `ClientTasks` registry (not a bare
        // `tokio::spawn`) so a shutdown sequence can genuinely AWAIT this
        // task's completion instead of guessing a fixed grace period
        // (round-10 review finding).
        tasks.lock().await.spawn(async move {
            let _permit = permit; // held for the connection's lifetime
            let _ip_guard = ip_guard; // held for the connection's lifetime
            // MAN-59 review: a socket error mid-session (a WS Pong write
            // failing, a raw TCP read resetting) returns Err, but every
            // OTHER disconnect path already logs its own specific reason
            // inline -- this is the one catch-all left uncovered without
            // it, and the only place that needs the raw error itself.
            if looks_like_websocket_handshake(&socket).await {
                ctx.metrics.inc_ws_clients();
                let result = handle_ws_client(
                    socket,
                    rx,
                    ctx.clone(),
                    peer,
                    peer_ip,
                    ip_ping_limiter,
                    log_enabled,
                    rejection_log_limiter,
                )
                .await;
                // MAN-68 (round 6): only the Unlogged variant reaches here
                // -- Logged means the handshake reject/timeout branch
                // already reported this exact error.
                if log_enabled {
                    if let Err(WsClientError::Unlogged(e)) = &result {
                        tracing::warn!(peer = %peer, error = %e, "json_stream: WS client task ended with an error");
                    }
                }
                ctx.metrics.dec_ws_clients();
            } else {
                ctx.metrics.inc_json_clients();
                let result = handle_tcp_client(socket, rx, ctx.clone(), peer, log_enabled).await;
                if log_enabled {
                    if let Err(e) = &result {
                        tracing::warn!(peer = %peer, error = %e, "json_stream: raw TCP client task ended with an error");
                    }
                }
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

#[tracing::instrument(
    name = "json_client",
    skip(socket, rx, ctx, log_enabled),
    fields(peer = %peer)
)]
async fn handle_tcp_client(
    mut socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
    peer: std::net::SocketAddr,
    log_enabled: bool,
) -> std::io::Result<()> {
    if log_enabled {
        tracing::info!("json_stream: raw TCP client connected");
    }
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
                            // MAN-59 review round 2: returns Ok(()), not
                            // Err -- log it directly.
                            if log_enabled {
                                tracing::warn!("json_stream: spot write failed, disconnecting");
                            }
                            ctx.metrics.record_write_failed(
                                crate::metrics::abandoned_spot_count(true, rx.len()),
                            );
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
                        if log_enabled {
                            tracing::warn!(lost, "json_stream: client lagged behind broadcast, disconnecting");
                        }
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
                        if log_enabled {
                            tracing::info!("json_stream: raw TCP client disconnected");
                        }
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
                        if log_enabled {
                            tracing::warn!("json_stream: unexpected client data on pure-push stream, disconnecting");
                        }
                        return Ok(());
                    }
                    // MAN-59 review: a genuine socket read error (e.g. a
                    // connection reset) was previously swallowed into
                    // Ok(()) here with no log at all -- the task-boundary
                    // catch-all this handler's caller now applies can only
                    // report the disconnect reason when the error actually
                    // propagates.
                    Err(e) => return Err(e),
                }
            }
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                // MAN-45 (round-16 finding): this loop's OWN deadline --
                // see `telnet::handle_client`'s identical shutdown-drain
                // branch, and `tasks::CLIENT_DRAIN_DEADLINE`'s doc comment,
                // for the full rationale (a flat outer registry-wide
                // deadline can't scale with any one client's backlog
                // depth).
                //
                // A `Lagged(n)` mid-drain means this subscriber missed `n`
                // spots, not that the channel is empty -- there can still
                // be spots queued after the gap. Stopping on the first
                // `Err` (the prior behavior) silently dropped everything
                // from that point on without even recording the loss
                // (round-6 review finding).
                let drain_deadline = tokio::time::Instant::now() + ctx.drain_deadline;
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            let line = ctx.render(&bus_spot);
                            // Checked BEFORE the write, not around it --
                            // see telnet's identical comment for why.
                            let remaining = drain_deadline
                                .saturating_duration_since(tokio::time::Instant::now());
                            let timed_out = remaining.is_zero()
                                || !matches!(
                                    tokio::time::timeout(WRITE_TIMEOUT.min(remaining), async {
                                        socket.write_all(line.as_bytes()).await?;
                                        socket.write_all(b"\n").await
                                    })
                                    .await,
                                    Ok(Ok(())),
                                );
                            if timed_out {
                                // The client's socket is presumably dead --
                                // further writes would just fail too, so
                                // stop draining and count what's abandoned
                                // (this failed spot plus anything still
                                // retained), rather than silently
                                // discarding the error and continuing to
                                // burn the write timeout on every remaining
                                // queued spot (round-12 review finding), or
                                // letting the outer registry-wide deadline
                                // abort this task mid-write once a
                                // multi-spot backlog exceeded it, also
                                // uncounted (round-16 review finding).
                                // MAN-59 review round 2: returns Ok(()),
                                // not Err -- log it directly.
                                if log_enabled {
                                    tracing::warn!(
                                        "json_stream: shutdown-drain write failed or ran out of budget, disconnecting"
                                    );
                                }
                                ctx.metrics.record_write_failed(
                                    crate::metrics::abandoned_spot_count(true, rx.len()),
                                );
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

/// MAN-68 (PR #85 review round 8): classifies a `ws.next()` error as a
/// genuine protocol violation (a malformed frame, oversized message, bad
/// UTF-8, tungstenite's own attack detection -- worth charging to the
/// dedicated rejection budget) versus a routine transport-level
/// disconnect (`ConnectionClosed`/`AlreadyClosed`/`Io`/`Tls` -- an
/// ordinary reset, unclean close, or I/O error: mobile-network/NAT/proxy
/// churn, not a security-relevant rejection). Split out so this
/// classification is unit-testable without a real WebSocket handshake,
/// same as `uplink`'s own precedent for pulling policy logic out of an
/// I/O-bound function.
fn is_ws_protocol_violation(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::{error::ProtocolError, Error as WsError};
    match e {
        // MAN-68 (round 8): a peer that drops the raw TCP connection
        // without sending a WS close frame surfaces as THIS specific
        // `Protocol` sub-variant, not as `Io`/`ConnectionClosed` --
        // tungstenite classifies it as a protocol-layer error even though
        // it's exactly the same routine mobile/NAT/proxy disconnect the
        // round-7/8 split already carves out for `Io`/`ConnectionClosed`.
        // A blanket `Protocol(_)` match would still charge every one of
        // these routine resets to the rejection budget, defeating the
        // fix's own purpose.
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => false,
        WsError::Protocol(_) | WsError::Capacity(_) | WsError::Utf8 | WsError::AttackAttempt => {
            true
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "ws_client",
    skip(
        socket,
        rx,
        ctx,
        peer_ip,
        ip_ping_limiter,
        log_enabled,
        rejection_log_limiter
    ),
    fields(peer = %peer)
)]
async fn handle_ws_client(
    socket: TcpStream,
    mut rx: broadcast::Receiver<BusSpot>,
    mut ctx: ClientCtx,
    peer: std::net::SocketAddr,
    peer_ip: IpAddr,
    ip_ping_limiter: IpRateLimiter,
    log_enabled: bool,
    rejection_log_limiter: IpRateLimiter,
) -> Result<(), WsClientError> {
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
            if log_enabled {
                tracing::warn!(error = %e, "json_stream: WS handshake rejected");
            }
            return Err(WsClientError::Logged);
        }
        Err(_) => {
            if log_enabled {
                tracing::warn!("json_stream: WS handshake timed out");
            }
            return Err(WsClientError::Logged);
        }
    };
    if log_enabled {
        tracing::info!("json_stream: WS client connected");
    }
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
                            // MAN-59 review round 2: returns Ok(()), not
                            // Err -- log it directly.
                            if log_enabled {
                                tracing::warn!("json_stream: WS spot write failed, disconnecting");
                            }
                            ctx.metrics.record_write_failed(
                                crate::metrics::abandoned_spot_count(true, rx.len()),
                            );
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // See the TCP handler's identical Lagged branch
                        // above for why `n` alone under-counts.
                        let lost = crate::bus::total_lag_loss(n, &rx);
                        if log_enabled {
                            tracing::warn!(lost, "json_stream: WS client lagged behind broadcast, disconnecting");
                        }
                        ctx.metrics.record_lagged(lost);
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => {
                        if log_enabled {
                            tracing::info!("json_stream: WS client disconnected");
                        }
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
                            // MAN-68 (round 7): the dedicated rejection
                            // budget, not `log_enabled` -- see
                            // REJECTION_LOG_MAX_PER_WINDOW's doc comment.
                            if rejection_log_limiter.allow(peer_ip) {
                                tracing::warn!("json_stream: client exceeded Ping rate budget, disconnecting");
                            }
                            return Ok(());
                        }
                        let write_result =
                            tokio::time::timeout(WRITE_TIMEOUT, ws.send(Message::Pong(payload)))
                                .await;
                        if !matches!(write_result, Ok(Ok(()))) {
                            // MAN-45 (round-16 review finding): a bare `?`
                            // here (the prior behavior) returned Err
                            // straight out of the handler, abandoning
                            // everything still retained in `rx` without
                            // counting any of it -- the last write site in
                            // this file or `telnet.rs` still doing that
                            // after rounds 11-15 converted the rest. No
                            // spot was in flight (this is a control frame),
                            // so only the retained backlog is charged --
                            // same shape as telnet's filter-ack site.
                            if log_enabled {
                                tracing::warn!("json_stream: WS Pong write failed, disconnecting");
                            }
                            ctx.metrics.record_write_failed(
                                crate::metrics::abandoned_spot_count(false, rx.len()),
                            );
                            return Ok(());
                        }
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
                        if log_enabled {
                            tracing::warn!("json_stream: unsolicited Pong on pure-push stream, disconnecting");
                        }
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
                        if log_enabled {
                            tracing::warn!("json_stream: unexpected data frame on pure-push stream, disconnecting");
                        }
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        // MAN-68 (round 8): `ws.next()` returning `Err`
                        // covers BOTH a genuine protocol violation (a
                        // malformed frame, oversized message, bad UTF-8)
                        // AND an ordinary transport-level disconnect (a
                        // reset, an unclean close, an I/O error) -- the
                        // latter is routine mobile-network/NAT/proxy
                        // churn, not a security-relevant rejection, and
                        // charging it to the dedicated rejection budget
                        // would let 30 routine disconnects from one IP
                        // exhaust the very budget this change exists to
                        // protect (the same class of gap round 7 already
                        // fixed for the Ping-budget/malformed-frame
                        // sites). Only a genuine protocol-shaped error
                        // consumes the rejection budget; a transport error
                        // is a routine lifecycle event.
                        let is_protocol_violation = is_ws_protocol_violation(&e);
                        let should_log = if is_protocol_violation {
                            rejection_log_limiter.allow(peer_ip)
                        } else {
                            log_enabled
                        };
                        if should_log {
                            if is_protocol_violation {
                                tracing::warn!(error = %e, "json_stream: malformed WS frame, disconnecting");
                            } else {
                                tracing::info!(error = %e, "json_stream: WS client disconnected (transport error)");
                            }
                        }
                        return Ok(());
                    }
                }
            }
            // Explicit shutdown: drain whatever's already queued rather
            // than dropping it when the runtime forcibly tears down.
            _ = ctx.shutdown.changed() => {
                // MAN-45 (round-16 finding): this loop's OWN deadline --
                // see the TCP handler's identical shutdown-drain branch
                // above, `telnet::handle_client`'s, and
                // `tasks::CLIENT_DRAIN_DEADLINE`'s doc comment, for the
                // full rationale. See the TCP handler's identical
                // shutdown-drain branch above for why `Lagged` must not
                // stop the drain.
                let drain_deadline = tokio::time::Instant::now() + ctx.drain_deadline;
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            let text = ctx.render(&bus_spot);
                            // Checked BEFORE the write, not around it --
                            // see telnet's identical comment for why.
                            let remaining = drain_deadline
                                .saturating_duration_since(tokio::time::Instant::now());
                            let timed_out = remaining.is_zero()
                                || !matches!(
                                    tokio::time::timeout(
                                        WRITE_TIMEOUT.min(remaining),
                                        ws.send(Message::Text(text.into())),
                                    )
                                    .await,
                                    Ok(Ok(())),
                                );
                            if timed_out {
                                // See the TCP handler's identical
                                // shutdown-drain branch for why a failed or
                                // budget-exhausted write stops the drain
                                // instead of silently continuing (round-12
                                // review finding), and does so with its own
                                // deadline rather than only the outer
                                // registry-wide one (round-16 review
                                // finding).
                                // MAN-59 review round 2: returns Ok(()),
                                // not Err -- log it directly.
                                if log_enabled {
                                    tracing::warn!(
                                        "json_stream: WS shutdown-drain write failed or ran out of budget, disconnecting"
                                    );
                                }
                                ctx.metrics.record_write_failed(
                                    crate::metrics::abandoned_spot_count(true, rx.len()),
                                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::{
        error::{CapacityError, ProtocolError},
        Error as WsError,
    };

    /// MAN-68, PR #85 review round 8: a genuine protocol-shaped error
    /// must be classified as a rejection worth its own audit budget.
    #[test]
    fn protocol_shaped_errors_are_classified_as_violations() {
        assert!(is_ws_protocol_violation(&WsError::Protocol(
            ProtocolError::WrongHttpMethod
        )));
        assert!(is_ws_protocol_violation(&WsError::Capacity(
            CapacityError::MessageTooLong {
                size: 100,
                max_size: 10,
            }
        )));
        assert!(is_ws_protocol_violation(&WsError::Utf8));
        assert!(is_ws_protocol_violation(&WsError::AttackAttempt));
    }

    /// A transport-level disconnect must NOT be classified as a
    /// violation -- these are routine mobile-network/NAT/proxy churn, and
    /// charging them to the dedicated rejection budget would defeat the
    /// whole point of round 7's fix (see `is_ws_protocol_violation`'s doc
    /// comment).
    #[test]
    fn transport_level_errors_are_not_classified_as_violations() {
        assert!(!is_ws_protocol_violation(&WsError::ConnectionClosed));
        assert!(!is_ws_protocol_violation(&WsError::AlreadyClosed));
        assert!(!is_ws_protocol_violation(&WsError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer")
        )));
    }

    /// MAN-68, PR #85 review round 8: a peer that drops the raw TCP
    /// connection without a WS close frame surfaces as this SPECIFIC
    /// `Protocol` sub-variant, not `Io`/`ConnectionClosed` -- a blanket
    /// `Protocol(_)` match (this function's first version) would still
    /// misclassify it as a rejection, letting routine disconnects exhaust
    /// the budget the round-7/8 split exists to protect.
    #[test]
    fn reset_without_closing_handshake_is_not_classified_as_a_violation() {
        assert!(!is_ws_protocol_violation(&WsError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        )));
    }
}
