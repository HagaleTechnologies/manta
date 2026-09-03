//! Outbound RBN telnet uplink -- MAN-32. Connects to RBN's own
//! spot-collection endpoint as a client and forwards manta's own
//! validated spots (`SpotBus`) in the same `DX de` wire format the
//! inbound telnet server (`telnet.rs`) emits, per ARCHITECTURE §7's wire
//! format and this repo's own reference implementation of that protocol
//! from the server side.

use crate::bounded_io::{read_line_bounded, read_line_bounded_with_timeout};
use crate::bus::SpotBus;
use crate::config::RbnUplinkConfig;
use crate::metrics::Metrics;
use crate::rate_limit::RateLimiter;
use crate::rbn;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Bounds `TcpStream::connect` (MAN-58 comment finding 1): a target that
/// silently black-holes SYNs (e.g. a firewall drop, not a refusal) would
/// otherwise leave this attempt pending for the OS's own connect timeout
/// (commonly minutes), blocking shutdown the whole time -- this attempt
/// is additionally raced against `shutdown.changed()` below, but the
/// timeout still bounds worst-case time when shutdown never fires.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Every outbound write to the uplink target gets this long before being
/// treated as stalled (MAN-58 comment finding 2) -- matches `telnet.rs`'s
/// identical `WRITE_TIMEOUT` for the same class of risk on the inbound
/// side: a target that completes login but stops reading (TCP receive
/// window fills, then the local send buffer fills) must not block this
/// task indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Response-line rate budget for the target's post-login discard read
/// (MAN-58 comment finding 3): RBN's collection server isn't expected to
/// send anything meaningful back after login, but with no budget at all
/// a misbehaving, compromised, or MITM'd target sending an endless stream
/// of short, valid, newline-terminated lines would keep this task hot
/// indefinitely with no CPU/bandwidth bound -- mirrors `telnet.rs`'s
/// `MAX_TELNET_COMMANDS`/`COMMAND_RATE_WINDOW` budget for the same class
/// of risk on the inbound side. A RATE, not a lifetime total: a
/// long-running connection where the target occasionally sends a stray
/// line must never be disconnected just for staying connected a long
/// time.
const MAX_TARGET_RESPONSE_LINES: u32 = 30;
const TARGET_RESPONSE_RATE_WINDOW: Duration = Duration::from_secs(60);

/// Whether a connection attempt got far enough to matter for backoff:
/// a connection that completed login before dropping was healthy, so the
/// *next* attempt should retry quickly rather than inherit backoff state
/// from an unrelated earlier outage. A connection that never got past
/// `TcpStream::connect`/login (target down, refusing, wrong port) hasn't
/// demonstrated that, so backoff keeps growing.
enum ConnectAttemptError {
    NeverConnected,
    Disconnected,
}

/// Pure backoff-transition function, split out so this policy is
/// unit-testable without any real sleeping/timing.
fn next_backoff(current: Duration, outcome: &ConnectAttemptError) -> Duration {
    match outcome {
        ConnectAttemptError::Disconnected => INITIAL_BACKOFF,
        ConnectAttemptError::NeverConnected => (current * 2).min(MAX_BACKOFF),
    }
}

/// Reconnect-with-backoff loop around one uplink connection. Never
/// returns while `config.enabled` and the shutdown signal hasn't fired --
/// a dropped connection must not permanently silence the uplink, since
/// that would defeat MAN-32's purpose of manta staying a live RBN
/// contributor.
pub async fn serve(
    config: RbnUplinkConfig,
    station_callsign: String,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    if !config.enabled {
        return;
    }
    let login_callsign = config
        .effective_login_callsign(&station_callsign)
        .to_string();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if *shutdown.borrow() {
            return;
        }

        match connect_and_forward(&config, &login_callsign, &bus, &metrics, &mut shutdown).await {
            Ok(()) => return, // clean shutdown-signaled exit
            Err(outcome) => {
                // No mark_uplink_disconnected() here: connect_and_forward
                // already paired its own mark_uplink_connected() with a
                // mark_uplink_disconnected() before returning Err (or
                // never marked connected at all, if it failed before
                // login completed) -- an extra call here would double-
                // decrement the shared count once other targets are also
                // marking it (MAN-42).
                metrics.record_uplink_reconnect();
                let sleep_for = backoff;
                backoff = next_backoff(backoff, &outcome);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Resolves `host:port` to every candidate address and gives each one its
/// own `CONNECT_TIMEOUT` attempt in turn (PR #80 review, round 2, finding
/// 2): `TcpStream::connect` alone tries every resolved address
/// internally, but a single timeout wrapped around the whole operation
/// can expire mid-attempt on the FIRST address, before Tokio ever
/// advances to a later, reachable one -- and since every retry
/// re-resolves and restarts from the first address again, a target whose
/// first resolved address black-holes SYNs (with a real one available
/// after it) could stay permanently unreachable. Returns the first
/// address that accepts, or the last error if every address failed or
/// timed out.
async fn connect_any_resolved_address(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let addrs: Vec<_> = tokio::net::lookup_host((host, port)).await?.collect();
    connect_first_reachable(addrs).await
}

/// Tries each address in `addrs`, in order, returning the first one that
/// accepts within `CONNECT_TIMEOUT`, or the last error if every address
/// failed or timed out. Split out from `connect_any_resolved_address` so
/// the "keep trying later addresses" behavior is testable directly
/// against a caller-supplied address list, without depending on real DNS
/// resolving to more than one address.
async fn connect_first_reachable(addrs: Vec<std::net::SocketAddr>) -> std::io::Result<TcpStream> {
    let mut last_err = None;
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ))
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses to try")
    }))
}

async fn connect_and_forward(
    config: &RbnUplinkConfig,
    login_callsign: &str,
    bus: &Arc<SpotBus>,
    metrics: &Arc<Metrics>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ConnectAttemptError> {
    // Raced against shutdown, not just time-bounded (MAN-58 comment
    // finding 1): a target that silently black-holes SYNs would otherwise
    // block shutdown for up to CONNECT_TIMEOUT (per address -- see
    // connect_any_resolved_address) even when shutdown fires immediately.
    // Looping rather than a single select arm: cancelling and retrying a
    // half-open `connect()` attempt has no partial state to lose (unlike
    // a buffered line read), so a spurious `changed()` wakeup with
    // `*shutdown.borrow() == false` just tries again.
    let stream = loop {
        tokio::select! {
            result = connect_any_resolved_address(config.target_host.as_str(), config.target_port) => {
                match result {
                    Ok(stream) => break stream,
                    Err(_) => return Err(ConnectAttemptError::NeverConnected),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    };
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    // Subscribe before completing login, matching telnet.rs's own
    // "subscribe before handshake" rule -- a spot published mid-login
    // must not be lost to the broadcast channel's no-history semantics.
    let mut rx = bus.subscribe();

    // Bounded (MAX_LINE_BYTES cap, via bounded_io) AND raced against
    // shutdown (MAN-58 finding 1): the prior bare `.await` had neither --
    // a target that accepted the connection but never sent a line hung
    // this task indefinitely and ignored shutdown signals. Looping, not a
    // single select arm: `read_line_bounded`'s own contract guarantees
    // bytes already consumed survive a losing race un-cleared in
    // `prompt_line`, so a spurious `changed()` wakeup with
    // `*shutdown.borrow() == false` resumes the same line rather than
    // losing progress.
    let mut prompt_line = String::new();
    loop {
        tokio::select! {
            result = read_line_bounded_with_timeout(&mut reader, &mut prompt_line) => {
                result.map_err(|_| ConnectAttemptError::NeverConnected)?;
                break;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
    write_with_timeout(&mut wr, format!("{login_callsign}\r\n").as_bytes())
        .await
        .map_err(|_| ConnectAttemptError::NeverConnected)?;

    metrics.mark_uplink_connected();
    let result = forward_loop(
        &mut reader,
        &mut wr,
        &mut rx,
        config,
        login_callsign,
        bus,
        metrics,
        shutdown,
    )
    .await;
    metrics.mark_uplink_disconnected();
    result.map_err(|_| ConnectAttemptError::Disconnected)
}

/// RBN's collection server isn't expected to send anything meaningful
/// back after login, but this task must still poll the read half --
/// otherwise a remote-side close (FIN) is invisible until the next
/// spot happens to be published and the resulting write fails. A node
/// that only notices it's disconnected whenever the next spot arrives
/// could sit silently un-contributing for an arbitrarily long gap.
#[allow(clippy::too_many_arguments)]
async fn forward_loop(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    rx: &mut broadcast::Receiver<crate::bus::BusSpot>,
    config: &RbnUplinkConfig,
    spotter_call: &str,
    bus: &Arc<SpotBus>,
    metrics: &Arc<Metrics>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut discard = String::new();
    let mut response_limiter =
        RateLimiter::new(MAX_TARGET_RESPONSE_LINES, TARGET_RESPONSE_RATE_WINDOW);
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(bus_spot) => {
                        if config.dry_run {
                            metrics.record_uplink_suppressed();
                            continue;
                        }
                        let unix_ts = bus.unix_ts_for(bus_spot.spot.sample_ts);
                        let line = rbn::format_line(&bus_spot.spot, spotter_call, unix_ts);
                        write_with_timeout(wr, format!("{line}\r\n").as_bytes()).await?;
                        metrics.record_uplink_sent();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        metrics.record_uplink_lagged(n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            // Bounded via bounded_io (MAN-58 finding 2): an unterminated
            // long line from the remote used to grow `discard` without
            // bound while this read was pending. No idle timeout here,
            // unlike the login-prompt read -- this branch is already
            // covered by the surrounding `select!`'s shutdown race, and
            // this connection is expected to sit quietly with nothing to
            // read for long stretches (a live spot uplink, not a
            // request/response protocol), matching telnet.rs's own
            // established/logged-in-client read (round-5 review finding
            // there against reusing the timed variant post-login).
            read_result = read_line_bounded(reader, &mut discard) => {
                match read_result {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "RBN uplink target closed the connection",
                        ));
                    }
                    Ok(_) => {
                        discard.clear();
                        // Every completed response line counts against
                        // the budget (MAN-58 comment finding 3), whether
                        // or not the target was expected to send it --
                        // an unbounded stream of otherwise-harmless lines
                        // is still unbounded CPU/bandwidth work.
                        if !response_limiter.allow() {
                            return Err(std::io::Error::other(
                                "RBN uplink target exceeded the response-line rate budget",
                            ));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

/// `wr.write_all` bounded by `WRITE_TIMEOUT` (MAN-58 comment finding 2),
/// matching `telnet.rs`'s identically-named helper for the inbound side.
async fn write_with_timeout(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    buf: &[u8],
) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "uplink write timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdTcpListener;

    #[test]
    fn backoff_resets_after_a_connection_that_reached_login() {
        let grown = Duration::from_secs(16);
        assert_eq!(
            next_backoff(grown, &ConnectAttemptError::Disconnected),
            INITIAL_BACKOFF
        );
    }

    #[test]
    fn backoff_doubles_and_caps_when_never_connected() {
        assert_eq!(
            next_backoff(Duration::from_secs(1), &ConnectAttemptError::NeverConnected),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff(
                Duration::from_secs(45),
                &ConnectAttemptError::NeverConnected
            ),
            MAX_BACKOFF
        );
    }

    #[test]
    fn effective_login_callsign_reflected_in_config() {
        // Coverage for RbnUplinkConfig::effective_login_callsign itself
        // lives in config.rs; this just confirms the module wiring
        // compiles against the real type. Behavior is exercised by the
        // integration tests in tests/uplink_acceptance.rs.
        let cfg = RbnUplinkConfig {
            enabled: true,
            target_host: "example.invalid".to_string(),
            target_port: 7300,
            login_callsign: None,
            dry_run: false,
        };
        assert_eq!(cfg.effective_login_callsign("W3XYZ"), "W3XYZ");
    }

    /// PR #80 review, round 2, finding 2: if the FIRST address in the
    /// list can't be connected to, `connect_first_reachable` must still
    /// try the next one rather than giving up entirely -- the actual bug
    /// this fixes (a single timeout wrapped around `TcpStream::connect`'s
    /// own internal multi-address fallback could expire on address 1
    /// before Tokio ever reached a reachable address 2). Uses a refused
    /// connection (a bound-then-dropped port) for "unreachable" rather
    /// than a real SYN black-hole, which would need the full
    /// `CONNECT_TIMEOUT` to fail -- both are address-1-fails-immediately-
    /// try-address-2 from this function's perspective.
    #[tokio::test]
    async fn connect_first_reachable_falls_through_to_a_later_working_address() {
        let refused_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let refused_addr = refused_listener.local_addr().unwrap();
        drop(refused_listener); // now nothing listens on this port: connection refused

        let good_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let good_addr = good_listener.local_addr().unwrap();

        let accept_task = std::thread::spawn(move || good_listener.accept().unwrap());

        let stream = connect_first_reachable(vec![refused_addr, good_addr])
            .await
            .expect("must fall through to the second, reachable address");
        assert_eq!(stream.peer_addr().unwrap(), good_addr);

        accept_task.join().unwrap();
    }

    #[tokio::test]
    async fn connect_first_reachable_errors_when_every_address_fails() {
        let refused_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let refused_addr = refused_listener.local_addr().unwrap();
        drop(refused_listener);

        let err = connect_first_reachable(vec![refused_addr])
            .await
            .expect_err("every address failing must be a real error, not a silent success");
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound); // a real connect error, not the empty-list fallback
    }
}
