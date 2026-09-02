//! Telnet DX-cluster server. ARCHITECTURE §7: "standard login prompt,
//! emits RBN-format spots... enough command grammar (`sh/dx`, filters) for
//! common clients not to choke." No real telnet IAC option negotiation --
//! real cluster nodes and clients (N1MM, stock `telnet`) work fine over
//! plain line-oriented text, and skipping IAC keeps this a small,
//! auditable text protocol (MAN-22/23 harden it further).

use crate::bounded_io::{read_line_bounded, read_line_bounded_with_timeout};
use crate::bus::SpotBus;
use crate::command::{self, Command};
use crate::metrics::Metrics;
use crate::rbn;
use crate::tasks::ClientTasks;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

/// Every outbound write gets this long before the client is treated as
/// stalled and disconnected -- ARCHITECTURE §7's "slow clients are
/// disconnected, never back-pressured" policy applies to a client that
/// stops reading, not just one that falls behind the broadcast channel.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the accept loop backs off after a failed `accept()` before
/// retrying -- a persistent resource error (e.g. `EMFILE`) makes
/// `accept()` return immediately, and retrying with no delay turns this
/// into a tight loop that starves other tasks on the same runtime.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
/// Command rate budget for an established (logged-in) client: at most this
/// many commands per `COMMAND_RATE_WINDOW`. A read-mostly protocol
/// legitimately sends very few commands (`sh/dx`, an occasional filter
/// change) -- but with no budget at all, an unauthenticated client could
/// send an unlimited sequence of complete commands (e.g. repeated
/// `sh/dx/50`), each one real CPU/bandwidth work formatting and writing up
/// to 50 history entries (round-14 review finding). A RATE, not a
/// lifetime total (see `json_stream`'s identical reasoning for its Ping
/// budget) -- a long session that occasionally issues commands must never
/// be disconnected just for staying connected a long time.
pub const MAX_TELNET_COMMANDS: u32 = 30;
pub const COMMAND_RATE_WINDOW: Duration = Duration::from_secs(10);

/// Accepts connections on `listener` until it errors, spawning one task
/// per client. Never returns under normal operation.
pub async fn serve(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    station_call: String,
    shutdown: watch::Receiver<bool>,
    tasks: ClientTasks,
) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => {
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        // Subscribe before spawning the connection task, not just before
        // the login handshake inside it -- `tokio::spawn` only schedules
        // the task, it doesn't guarantee it's polled before this loop
        // moves on to accept the next connection. A spot published after
        // `accept()` succeeds but before the spawned task is first polled
        // would otherwise be lost to the broadcast channel's no-history-
        // for-late-subscribers semantics on a busy runtime or a high-rate
        // stream (round-7 review finding; same fix applied to
        // `json_stream::serve`'s accept loop).
        let rx = bus.subscribe();
        let bus = bus.clone();
        let metrics = metrics.clone();
        let station_call = station_call.clone();
        let shutdown = shutdown.clone();
        // Tracked in the shared `ClientTasks` registry (not a bare
        // `tokio::spawn`) so a shutdown sequence can genuinely AWAIT this
        // task's completion instead of guessing a fixed grace period
        // (round-10 review finding).
        tasks.lock().await.spawn(async move {
            metrics.inc_telnet_clients();
            let _ = handle_client(socket, bus, rx, metrics.clone(), station_call, shutdown).await;
            metrics.dec_telnet_clients();
        });
    }
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    mut rx: broadcast::Receiver<crate::bus::BusSpot>,
    metrics: Arc<Metrics>,
    station_call: String,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    write_with_timeout(&mut wr, b"login: \r\n").await?;
    let mut login_line = String::new();
    if read_line_bounded_with_timeout(&mut reader, &mut login_line).await? == 0 {
        return Ok(()); // client hung up before logging in
    }

    write_with_timeout(&mut wr, format!("de {station_call}-# >\r\n").as_bytes()).await?;

    // `sh/dx` default when the client didn't specify a count.
    const DEFAULT_SHOW_DX_COUNT: usize = 10;
    let mut min_unique: Option<u32> = None;
    // Not cleared at the top of the loop, deliberately: `tokio::select!`
    // can cancel `read_line_bounded_with_timeout` mid-line (a spot arrived
    // first), and the bytes it already consumed from `reader` were
    // already appended into `cmd_line` as a side effect before that
    // cancellation point -- clearing here would discard them, silently
    // truncating the command to whatever chunk arrives next. Only cleared
    // once a full line has actually been parsed, below.
    let mut cmd_line = String::new();
    let mut command_limiter =
        crate::rate_limit::RateLimiter::new(MAX_TELNET_COMMANDS, COMMAND_RATE_WINDOW);
    loop {
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        if let Some(min) = min_unique {
                            if bus_spot.occurrence_count <= min {
                                metrics.record_filter_suppressed(1);
                                continue; // below threshold: filtered out
                            }
                        }
                        if write_spot_line(&mut wr, &bus, &station_call, &bus_spot.spot)
                            .await
                            .is_err()
                        {
                            // The write for THIS spot failed, plus
                            // whatever's still retained in `rx` is
                            // abandoned along with it -- both must be
                            // counted, not just a Lagged-induced loss
                            // (round-11 review finding).
                            metrics.record_write_failed(1 + rx.len() as u64);
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // ARCHITECTURE §7: slow clients are disconnected,
                        // never back-pressured -- and ARCHITECTURE §8:
                        // every dropped item is counted, not silent. `n`
                        // alone under-counts what's still retained in
                        // `rx`'s own buffer that this disconnect abandons
                        // too (round-9 review finding).
                        metrics.record_lagged(crate::bus::total_lag_loss(n, &rx));
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            // Deliberately the UNTIMED variant: this branch is polled every
            // trip through the loop, including while the client is
            // legitimately just listening for spots with nothing to say
            // for minutes at a time (a read-mostly protocol -- see
            // ARCHITECTURE §7). `IDLE_READ_TIMEOUT` only guards login
            // (above, via the timed variant) and an in-progress partial
            // command line -- an established, quietly-listening client
            // must never be disconnected just for staying quiet. (Round-5
            // review finding: this branch used to reuse the timed variant
            // here too, which cut off exactly that client after 30s.)
            n = read_line_bounded(&mut reader, &mut cmd_line) => {
                if n? == 0 {
                    return Ok(()); // client disconnected
                }
                // Every completed command line counts against the budget,
                // regardless of what it parses to (an unrecognized line
                // still costs a parse + a select! iteration) -- an
                // unlimited sequence of e.g. `sh/dx/50` is real CPU/
                // bandwidth work, not free (round-14 review finding).
                if !command_limiter.allow() {
                    return Ok(());
                }
                match command::parse(&cmd_line) {
                    Command::ShowDx { count } => {
                        let n = count.unwrap_or(DEFAULT_SHOW_DX_COUNT);
                        // Apply the SAME `min_unique` predicate the live
                        // stream uses -- a spot suppressed live must stay
                        // suppressed when replayed via `sh/dx`, not leak
                        // through unfiltered and uncounted (round-11
                        // review finding). `bus.recent` carries each
                        // spot's publish-time occurrence_count precisely
                        // so this comparison is possible here.
                        let mut history = bus.recent(n).into_iter();
                        while let Some(bus_spot) = history.next() {
                            if let Some(min) = min_unique {
                                if bus_spot.occurrence_count <= min {
                                    metrics.record_filter_suppressed(1);
                                    continue;
                                }
                            }
                            if write_spot_line(&mut wr, &bus, &station_call, &bus_spot.spot)
                                .await
                                .is_err()
                            {
                                // A bare `?` here (the prior behavior)
                                // abandoned not just the rest of this
                                // history replay but every live spot
                                // still retained in `rx` too, uncounted
                                // (round-13 review finding) -- count this
                                // failed write, whatever's left of the
                                // history iterator, and whatever's still
                                // retained on the live channel.
                                metrics.record_write_failed(
                                    1 + history.len() as u64 + rx.len() as u64,
                                );
                                return Ok(());
                            }
                        }
                    }
                    Command::SetFilterUnique { min } => {
                        min_unique = Some(min);
                        write_with_timeout(&mut wr, format!("Filter set: unique > {min}\r\n").as_bytes())
                            .await?;
                    }
                    // Read-mostly protocol: any other line (unrecognized
                    // commands the client sent) is accepted without
                    // choking the connection.
                    Command::Unknown => {}
                }
                cmd_line.clear(); // a full line was consumed and processed
            }
            // Explicit shutdown signal (not just letting the runtime's
            // forced-timeout abort us): drain whatever's already queued
            // on the broadcast channel -- e.g. spots TrackManager::finish()
            // published right before the daemon exited -- rather than
            // dropping them unsent.
            _ = shutdown.changed() => {
                // A `Lagged(n)` mid-drain means this subscriber missed `n`
                // spots, not that the channel is empty -- there can still
                // be spots queued after the gap. Stopping on the first
                // `Err` (the prior behavior) silently dropped everything
                // from that point on without even recording the loss
                // (round-6 review finding).
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            if let Some(min) = min_unique {
                                if bus_spot.occurrence_count <= min {
                                    metrics.record_filter_suppressed(1);
                                    continue;
                                }
                            }
                            if write_spot_line(&mut wr, &bus, &station_call, &bus_spot.spot)
                                .await
                                .is_err()
                            {
                                // The client's socket is presumably dead --
                                // further writes would just fail too. A
                                // bare `?` here (the prior behavior)
                                // propagated the error out of the whole
                                // handler, abandoning the rest of the
                                // drain loop uncounted (round-12 review
                                // finding).
                                metrics.record_write_failed(1 + rx.len() as u64);
                                return Ok(());
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            metrics.record_lagged(n);
                        }
                        Err(_) => break, // Empty or Closed: nothing left to drain
                    }
                }
                return Ok(());
            }
        }
    }
}

async fn write_spot_line(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    bus: &SpotBus,
    station_call: &str,
    spot: &manta_spot::Spot,
) -> std::io::Result<()> {
    let unix_ts = bus.unix_ts_for(spot.sample_ts);
    let line = rbn::format_line(spot, station_call, unix_ts);
    write_with_timeout(wr, line.as_bytes()).await?;
    write_with_timeout(wr, b"\r\n").await
}

async fn write_with_timeout(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    buf: &[u8],
) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))?
}
