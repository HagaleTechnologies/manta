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
use crate::rate_limit::IpRateLimiter;
use crate::rbn;
use crate::tasks::{ClientTasks, ConnectionLimiter, IpQuota};
use std::net::IpAddr;
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
/// Upper bound on concurrently admitted telnet clients. With no cap at all,
/// an unauthenticated client could open connections without bound, each one
/// costing a socket, a tracked task, and its own broadcast subscription
/// that every future publish must additionally fan out to (round-15 review
/// finding). Generous headroom over any realistic legitimate DX-cluster
/// client count.
pub const MAX_TELNET_CONNECTIONS: usize = 512;
/// Upper bound on concurrently admitted telnet clients from a SINGLE
/// source IP (MAN-61, `docs/DECISIONS/2026-09-03-man61-per-ip-connection-
/// quota.md`): `MAX_TELNET_CONNECTIONS` alone bounds the total across
/// every client combined, but a telnet client retains its permit
/// indefinitely once logged in (below) with nothing further required of
/// it -- one source could otherwise open up to `MAX_TELNET_CONNECTIONS`
/// connections, send nothing further, and permanently deny admission to
/// every other client. 16 leaves room for a handful of legitimate
/// multi-connection uses behind one IP (NAT, a monitoring tool opening
/// more than one session) while still requiring at least 32 distinct
/// sources to exhaust the full 512-connection ceiling.
pub const MAX_TELNET_CONNECTIONS_PER_IP: usize = 16;

// MAN-57: `command_limiter` below is per-CONNECTION, so a source opening
// several connections (up to `MAX_TELNET_CONNECTIONS_PER_IP`) gets that
// many independent full command budgets -- the aggregate effective rate
// from one IP is up to 16x the intended single-connection budget, not the
// budget itself. `serve`'s `ip_command_limiter` parameter below is a
// shared, IP-keyed sibling checked in addition to each connection's own,
// using the SAME budget: the intent (from
// `MAX_TELNET_COMMANDS`/`COMMAND_RATE_WINDOW`'s own reasoning) was always
// "this many commands per source in this window", not "per connection" --
// opening more connections must not multiply it.

/// MAN-59 review round 2: the per-IP quota-rejection warning below runs
/// on every rejected socket, BEFORE any request/command rate limiter --
/// nothing bounds how often it fires. A source that holds its allotment
/// and keeps completing new TCP handshakes anyway could otherwise flood
/// or block the log sink at whatever rate the OS lets it open sockets,
/// unrelated to and unbounded by `MAX_TELNET_CONNECTIONS_PER_IP`. Caps
/// this one specific event to once per source per window; the actual
/// rejection behavior (declining the connection) is unaffected either
/// way -- only how often it gets LOGGED is throttled.
const QUOTA_REJECT_LOG_MAX_PER_WINDOW: u32 = 1;
const QUOTA_REJECT_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-59 review round 4: rounds 2-3 each found one more individually
/// un-gated audit-log call site (the quota-reject warning, the 404
/// warning, a missing task-boundary catch-all) -- a genuinely new shape
/// of the SAME gap three rounds running, per
/// `resolve-review-feedback`'s convergence policy's own "reconsider the
/// fix strategy, not another point patch" signal. Round 4's own finding
/// makes the underlying issue explicit: `QUOTA_REJECT_LOG_*` above only
/// covers connections REJECTED for being over quota -- a source that
/// stays under the concurrent quota by connecting and disconnecting
/// quickly (e.g. right after the login prompt) was never gated by
/// anything, and each such cycle unconditionally logs at least a
/// `connected` and a `disconnected` event. Rather than hunting for and
/// patching each individual log call in `handle_client` one at a time,
/// ONE budget is decided once per ADMITTED connection (`serve` below,
/// same instant the connection is spawned) and threaded through as
/// `log_enabled` -- every tracing call in that connection's entire
/// lifetime (connect, login, commands, disconnect, write failures, the
/// task-boundary catch-all) checks the SAME decision, so a future call
/// site can't be added without it needing an explicit unrated bypass, and
/// a source combining several previously-separately-gated event types
/// can no longer sum their individual budgets. 30 per window (not 1, like
/// the pure-rejection limiter above) -- unlike a rejection, an admitted
/// connection is legitimate use by construction, so this only needs to
/// bound CHURN RATE, not suppress routine multi-connection activity from
/// one real source (NAT, a monitoring tool).
const CONNECTION_LOG_MAX_PER_WINDOW: u32 = 30;
const CONNECTION_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-68 (PR #85 review round 7): `log_enabled` above is a single budget
/// covering BOTH routine connection-lifecycle noise (connect/disconnect/
/// login/command-received) AND genuinely security-relevant rejections
/// (a client exceeding its command-rate budget). Ordinary churn --
/// opening and closing many harmless connections quickly, e.g. behind the
/// documented reverse-proxy deployment where every downstream client
/// shares one IP -- can exhaust that shared budget on its own, after
/// which a genuinely malicious 31st+ connection's rejection goes entirely
/// unrecorded even though rejections are the LOW-volume, high-value half
/// of the audit trail (a real rejection IS the disconnect reason; it's
/// not noise to be throttled alongside routine churn). This separate,
/// per-IP budget is checked ONLY at the command-rate-budget-exceeded site
/// (`handle_client` below) -- independent of `log_enabled` -- so that
/// site keeps logging even once ordinary lifecycle churn has exhausted
/// the shared budget above. Same cap/window as `CONNECTION_LOG_*`: this
/// budget is never touched by harmless churn at all (only an actual
/// command-rate violation decrements it), so it doesn't need to be
/// larger to stay effective.
const REJECTION_LOG_MAX_PER_WINDOW: u32 = 30;
const REJECTION_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-68 (PR #85 review round 6): the login-read and command-read Err
/// branches in `handle_client` already log a specific rejection warning
/// before returning `Err` -- `serve`'s task-boundary catch-all then logged
/// a SECOND, generic warning for the same `Err`, so one real rejection
/// produced two audit lines. `ClientError` lets the catch-all tell the
/// two cases apart: `Logged` for an error a specific branch already
/// reported (the catch-all skips it), `Unlogged` for anything else. The
/// blanket `From<std::io::Error>` impl below defaults every OTHER
/// fallible site (the bare `?` writes) to `Unlogged`, so they keep
/// reaching the catch-all exactly as before -- a future fallible call
/// site added without an explicit `Logged` wrap still gets caught by it,
/// preserving MAN-59's original "no disconnect goes unrecorded" guarantee
/// rather than silently losing it along with the double-log fix.
enum ClientError {
    /// The specific branch that produced this already logged it -- no
    /// error payload carried, since nothing downstream needs it.
    Logged,
    Unlogged(std::io::Error),
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Unlogged(e)
    }
}

/// Accepts connections on `listener` until it errors, spawning one task
/// per client. Never returns under normal operation.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    station_call: String,
    shutdown: watch::Receiver<bool>,
    tasks: ClientTasks,
    limiter: ConnectionLimiter,
    ip_quota: IpQuota,
    ip_command_limiter: IpRateLimiter,
    drain_deadline: Duration,
) {
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
                tracing::warn!(ip = %peer.ip(), "telnet: per-IP connection quota exceeded, declining");
            }
            continue;
        };
        // Blocks the accept loop itself (not just the client) until
        // capacity is available -- a flood beyond `MAX_TELNET_CONNECTIONS`
        // is left waiting in the OS's own connection backlog rather than
        // ever being admitted, tracked, or given a broadcast subscription
        // at all (round-15 review finding).
        let Ok(permit) = limiter.clone().acquire_owned().await else {
            continue; // limiter closed: unreachable in practice, never panics
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
        let peer_ip = peer.ip();
        let ip_command_limiter = ip_command_limiter.clone();
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
            metrics.inc_telnet_clients();
            let result = handle_client(
                socket,
                bus,
                rx,
                metrics.clone(),
                station_call,
                shutdown,
                peer,
                peer_ip,
                ip_command_limiter,
                log_enabled,
                rejection_log_limiter,
                drain_deadline,
            )
            .await;
            // MAN-59 review: a socket error mid-session (e.g. a
            // login-prompt write reset) returns Err, but every OTHER
            // disconnect path already logs its own specific reason
            // inline -- this is the one catch-all left uncovered without
            // it, and the only place that needs the raw io::Error itself.
            // MAN-68 (round 6): only the Unlogged variant reaches here --
            // Logged means a specific branch (login/command read
            // rejection) already reported this exact error, and a second
            // generic warning for it would just double the audit record.
            if log_enabled {
                if let Err(ClientError::Unlogged(e)) = &result {
                    tracing::warn!(peer = %peer, error = %e, "telnet: client task ended with an error");
                }
            }
            metrics.dec_telnet_clients();
        });
    }
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "telnet_client",
    skip(
        socket,
        bus,
        rx,
        metrics,
        station_call,
        shutdown,
        peer_ip,
        ip_command_limiter,
        log_enabled,
        rejection_log_limiter
    ),
    fields(peer = %peer)
)]
async fn handle_client(
    socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    mut rx: broadcast::Receiver<crate::bus::BusSpot>,
    metrics: Arc<Metrics>,
    station_call: String,
    mut shutdown: watch::Receiver<bool>,
    peer: std::net::SocketAddr,
    peer_ip: IpAddr,
    ip_command_limiter: IpRateLimiter,
    log_enabled: bool,
    rejection_log_limiter: IpRateLimiter,
    drain_deadline: Duration,
) -> Result<(), ClientError> {
    if log_enabled {
        tracing::info!("telnet: client connected");
    }
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    write_with_timeout(&mut wr, b"login: \r\n").await?;
    let mut login_line = String::new();
    match read_line_bounded_with_timeout(&mut reader, &mut login_line).await {
        Ok(0) => {
            if log_enabled {
                tracing::info!("telnet: client disconnected before completing login");
            }
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => {
            if log_enabled {
                tracing::warn!(error = %e, "telnet: login read rejected (oversized/malformed line or timeout), disconnecting");
            }
            return Err(ClientError::Logged);
        }
    }
    // MAN-59 review: the login line is client-supplied and unvalidated --
    // Display (`%`) writes it into the log verbatim, letting an
    // unauthenticated client embed CRs/ANSI escapes to forge additional
    // bogus log lines or manipulate terminal output. Debug (`?`) escapes
    // control characters instead.
    if log_enabled {
        tracing::info!(login = ?login_line.trim(), "telnet: client logged in");
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
                            // MAN-59 review round 2: this returns Ok(()),
                            // not Err, so the task-boundary catch-all
                            // never sees it -- log it directly.
                            if log_enabled {
                                tracing::warn!("telnet: spot write failed, disconnecting");
                            }
                            metrics.record_write_failed(crate::metrics::abandoned_spot_count(
                                true,
                                rx.len(),
                            ));
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
                        let lost = crate::bus::total_lag_loss(n, &rx);
                        if log_enabled {
                            tracing::warn!(lost, "telnet: client lagged behind broadcast, disconnecting");
                        }
                        metrics.record_lagged(lost);
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
                let n = match n {
                    Ok(n) => n,
                    Err(e) => {
                        if log_enabled {
                            tracing::warn!(error = %e, "telnet: command read rejected (oversized/malformed line), disconnecting");
                        }
                        return Err(ClientError::Logged);
                    }
                };
                if n == 0 {
                    if log_enabled {
                        tracing::info!("telnet: client disconnected");
                    }
                    return Ok(()); // client disconnected
                }
                // Every completed command line counts against the budget,
                // regardless of what it parses to (an unrecognized line
                // still costs a parse + a select! iteration) -- an
                // unlimited sequence of e.g. `sh/dx/50` is real CPU/
                // bandwidth work, not free (round-14 review finding).
                // Checked in addition to (never instead of) the
                // per-connection budget above -- MAN-57: without this, a
                // source opening several connections gets an independent
                // full budget on each one, multiplying the intended
                // per-source rate by however many connections it holds.
                if !command_limiter.allow() || !ip_command_limiter.allow(peer_ip) {
                    // MAN-68 (round 7): checked against the dedicated
                    // rejection budget, NOT `log_enabled` -- this is a
                    // genuine security-relevant rejection, which must not
                    // go unrecorded just because unrelated routine
                    // connection churn already exhausted the shared
                    // lifecycle log budget.
                    if rejection_log_limiter.allow(peer_ip) {
                        tracing::warn!("telnet: client exceeded command rate budget, disconnecting");
                    }
                    return Ok(());
                }
                // MAN-59 review: a client staying within the rate budget
                // could otherwise disconnect with zero command activity
                // recorded, leaving the audit trail unable to reconstruct
                // what happened -- only the over-budget disconnect above
                // was logged. Logs the PARSED, normalized command
                // (`Command`'s own Debug -- an enum variant plus already-
                // validated numeric fields, e.g. `ShowDx { count: Some(50) }`
                // or bare `Unknown`), never the raw client-supplied line,
                // which is unescaped and could otherwise inject the same
                // way the login field could (see the fix just above).
                let parsed_command = command::parse(&cmd_line);
                if log_enabled {
                    tracing::info!(command = ?parsed_command, "telnet: command received");
                }
                match parsed_command {
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
                                // MAN-59 review round 2: returns Ok(()),
                                // not Err -- log it directly.
                                if log_enabled {
                                    tracing::warn!("telnet: sh/dx history write failed, disconnecting");
                                }
                                metrics.record_write_failed(crate::metrics::abandoned_spot_count(
                                    true,
                                    history.len() + rx.len(),
                                ));
                                return Ok(());
                            }
                        }
                    }
                    Command::SetFilterUnique { min } => {
                        min_unique = Some(min);
                        if write_with_timeout(
                            &mut wr,
                            format!("Filter set: unique > {min}\r\n").as_bytes(),
                        )
                        .await
                        .is_err()
                        {
                            // A bare `?` here (the prior behavior) left
                            // whatever's queued on the live channel
                            // abandoned uncounted -- the same accounting
                            // gap every other write site in this file
                            // already closed (round-15 review finding).
                            // The failed write itself isn't a queued spot,
                            // so only the retained live-channel backlog
                            // counts here (no `1 +`).
                            // MAN-59 review round 2: returns Ok(()), not
                            // Err -- log it directly.
                            if log_enabled {
                                tracing::warn!("telnet: filter-ack write failed, disconnecting");
                            }
                            metrics.record_write_failed(crate::metrics::abandoned_spot_count(
                                false,
                                rx.len(),
                            ));
                            return Ok(());
                        }
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
                // MAN-45 (round-16 finding): this loop's OWN deadline. The
                // outer `SHUTDOWN_DRAIN_DEADLINE`/`await_all` bound is
                // registry-wide and cannot scale with any one client's
                // backlog depth -- and when it expired,
                // `Runtime::shutdown_timeout` aborted this task mid-write
                // with everything abandoned uncounted. Same monotonic
                // remaining-budget idiom `looks_like_websocket_handshake`
                // already uses in `json_stream`.
                //
                // A `Lagged(n)` mid-drain means this subscriber missed `n`
                // spots, not that the channel is empty -- there can still
                // be spots queued after the gap. Stopping on the first
                // `Err` (the prior behavior) silently dropped everything
                // from that point on without even recording the loss
                // (round-6 review finding).
                let drain_deadline = tokio::time::Instant::now() + drain_deadline;
                loop {
                    match rx.try_recv() {
                        Ok(bus_spot) => {
                            if let Some(min) = min_unique {
                                if bus_spot.occurrence_count <= min {
                                    metrics.record_filter_suppressed(1);
                                    continue; // a filtered spot costs no budget
                                }
                            }
                            // Checked BEFORE the write, not around it: this
                            // spot has already left `rx`, so a `timeout`
                            // wrapped around the whole loop would drop it
                            // mid-write -- neither delivered nor counted,
                            // reintroducing the silent loss this fix exists
                            // to end. `true` (in-flight) is exactly that
                            // spot.
                            let remaining = drain_deadline
                                .saturating_duration_since(tokio::time::Instant::now());
                            let timed_out = remaining.is_zero()
                                || !matches!(
                                    tokio::time::timeout(
                                        remaining,
                                        write_spot_line(&mut wr, &bus, &station_call, &bus_spot.spot),
                                    )
                                    .await,
                                    Ok(Ok(())),
                                );
                            if timed_out {
                                // The client's socket is presumably dead
                                // (or hopelessly slow) -- further writes
                                // would just fail or exhaust the budget
                                // too. A bare `?` here (the pre-round-12
                                // behavior) propagated the error out of the
                                // whole handler, abandoning the rest of the
                                // drain loop uncounted (round-12 review
                                // finding); a flat outer deadline alone
                                // (the pre-round-16 behavior) let
                                // `Runtime::shutdown_timeout` abort this
                                // task mid-write once a multi-spot backlog
                                // exceeded it, also uncounted (round-16
                                // review finding).
                                if log_enabled {
                                    tracing::warn!(
                                        "telnet: shutdown-drain write failed or ran out of budget, disconnecting"
                                    );
                                }
                                metrics.record_write_failed(crate::metrics::abandoned_spot_count(
                                    true,
                                    rx.len(),
                                ));
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
