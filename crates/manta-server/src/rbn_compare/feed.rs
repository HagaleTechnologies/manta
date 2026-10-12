//! MAN-301: the read-only RBN telnet feed client. Connects to
//! `[rbn_compare].feed_host:feed_port` (RBN's public CW/RTTY relay,
//! `telnet.reversebeacon.net:7000`), logs in with a callsign, and turns
//! every CW `DX de` line ([`super::ak1a`]) into an [`RbnReport`] stamped with
//! its **arrival** time: the relay's `HHMMZ` field carries no date, and RBN
//! relays within seconds, small against the 180 s comparison window.
//!
//! Read-only toward RBN: the write half is used exactly once, for
//! `login_callsign` + CRLF. No commands, no filters, no spots.
//!
//! The login read is deliberately NOT `uplink.rs`'s. MAN-90's plan
//! reproduced finding C7: a login prompt with no trailing newline
//! (manta's own telnet server sends `Please enter your callsign: `, and
//! DX-cluster prompts conventionally do the same) leaves the uplink's
//! line read waiting out its 30 s idle timeout and reconnecting forever.
//! Here the login goes out as soon as a line ends, the unterminated buffer
//! ends in `:` plus optional spaces, or [`LOGIN_PROMPT_WAIT`] passes with
//! nothing received -- whichever comes first. What RBN's relay actually
//! sends is unverified from this repo (no session here logs in to RBN);
//! the MAN-301 decision record carries it as an assumption.
//!
//! Connection handling follows `uplink.rs`: the same bounded connect
//! (`connect_first_reachable_bounded` with uplink's timeouts), the shared
//! `backoff.rs` ladder, and every await raced against shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{watch, Semaphore};
use tokio::time::Instant;

use super::ak1a::{self, LineKind};
use super::grade::RbnReport;
use super::Clock;
use crate::backoff::{next_backoff, AttemptOutcome, INITIAL_BACKOFF};
use crate::bounded_io::{read_line_bounded, MAX_LINE_BYTES};
use crate::metrics::Metrics;
use crate::uplink::{connect_first_reachable_bounded, CONNECT_TIMEOUT, OVERALL_CONNECT_TIMEOUT};

/// How long the feed may stay silent after connecting before the login
/// goes out anyway (a server that prompts with nothing at all).
pub const LOGIN_PROMPT_WAIT: Duration = Duration::from_secs(10);
/// No line for this long after login means the feed is lost: RBN's CW feed
/// is never silent for a minute.
pub const FEED_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds the one login write, as `uplink.rs`'s `WRITE_TIMEOUT` does: a
/// server that accepts but never reads must not wedge the task.
const LOGIN_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the feed tells the live comparator.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedEvent {
    /// The first `DX de` line of a session arrived: from `at` (Unix
    /// seconds) the feed covers RBN's reports.
    Up { at: i64 },
    /// One CW report, `time` = its arrival time.
    Report(RbnReport),
    /// A session that was [`FeedEvent::Up`] ended (idle timeout, close,
    /// read error, oversized line) at `at`. Not sent on shutdown.
    Down { at: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeedConfig {
    pub host: String,
    pub port: u16,
    pub login_callsign: String,
}

/// Why a session ended without a shutdown.
enum Lost {
    Connect(std::io::Error),
    ClosedBeforeLogin,
    PromptTooLong,
    LoginWrite(String),
    Idle,
    Closed,
    Read(std::io::Error),
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lost::Connect(e) => write!(f, "connect failed: {e}"),
            Lost::ClosedBeforeLogin => write!(f, "closed before the login prompt"),
            Lost::PromptTooLong => write!(
                f,
                "login prompt longer than {MAX_LINE_BYTES} bytes with no line end"
            ),
            Lost::LoginWrite(e) => write!(f, "login write failed: {e}"),
            Lost::Idle => write!(f, "no line for {} s", FEED_IDLE_TIMEOUT.as_secs()),
            Lost::Closed => write!(f, "connection closed by the feed"),
            Lost::Read(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                write!(f, "bad line from the feed: {e}")
            }
            Lost::Read(e) => write!(f, "read error: {e}"),
        }
    }
}

enum SessionEnd {
    Shutdown,
    Lost(Lost),
}

/// Runs the feed until `shutdown` turns true (or its sender is dropped).
/// Reconnects forever otherwise. Every CW report goes to `sink` as a
/// [`FeedEvent::Report`], bracketed by [`FeedEvent::Up`] (first `DX de`
/// line of a session) and [`FeedEvent::Down`] (that session lost).
/// Reports arrive in order on the calling task; `sink` must not block.
pub async fn run(
    cfg: FeedConfig,
    clock: Clock,
    mut sink: impl FnMut(FeedEvent) + Send,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    // One outstanding DNS lookup at a time, as `uplink.rs`'s per-target
    // `resolver_slot`: a stalled resolver must not pile abandoned
    // `getaddrinfo` jobs onto the blocking pool across retries.
    let resolver_slot = Arc::new(Semaphore::new(1));
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut up = false;
        let mut last_line_at = 0;
        let end = session(
            &cfg,
            &clock,
            &mut sink,
            &metrics,
            &mut shutdown,
            &resolver_slot,
            &mut up,
            &mut last_line_at,
        )
        .await;
        let lost = match end {
            SessionEnd::Shutdown => {
                if up {
                    metrics.set_rbn_feed_connected(false);
                }
                return;
            }
            SessionEnd::Lost(lost) => lost,
        };
        metrics.inc_rbn_feed_reconnects();
        // `uplink.rs`'s ordering: after a session that reached Up, retry
        // on the reset 1 s rung; a never-up attempt sleeps its current
        // rung, then grows it (1 s, 2 s, 4 s ... 60 s).
        let outcome = if up {
            AttemptOutcome::Disconnected
        } else {
            AttemptOutcome::NeverConnected
        };
        let sleep_for = match outcome {
            AttemptOutcome::Disconnected => INITIAL_BACKOFF,
            AttemptOutcome::NeverConnected => backoff,
        };
        backoff = next_backoff(sleep_for, &outcome);
        if up {
            // Stamped at the last line received, not now: an idle timeout
            // fires a whole minute after the feed went quiet, and that
            // minute delivered no reports, so it must not count as covered.
            sink(FeedEvent::Down { at: last_line_at });
            metrics.set_rbn_feed_connected(false);
            tracing::warn!(
                "rbn_compare: feed lost ({lost}); spots whose window overlaps the gap grade \
                 unknown unless RBN matched them; reconnecting in {} s",
                sleep_for.as_secs()
            );
        } else {
            tracing::warn!(
                "rbn_compare: no feed from {}:{} ({lost}); spots grade unknown until it \
                 delivers; reconnecting in {} s",
                cfg.host,
                cfg.port,
                sleep_for.as_secs()
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = shutdown_signalled(&mut shutdown) => return,
        }
    }
}

/// Completes once shutdown is true, or its sender is gone (nobody can
/// ask for one any more, and a dropped sender would otherwise make
/// `changed()` return at once forever). Cancel-safe.
async fn shutdown_signalled(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// One connection: connect, log in, read lines until lost. Sets `*up` at
/// the first `DX de` line so the caller knows whether to send `Down`, and
/// `*last_line_at` at every line so `Down` can be stamped with it.
#[allow(clippy::too_many_arguments)]
async fn session(
    cfg: &FeedConfig,
    clock: &Clock,
    sink: &mut (impl FnMut(FeedEvent) + Send),
    metrics: &Metrics,
    shutdown: &mut watch::Receiver<bool>,
    resolver_slot: &Arc<Semaphore>,
    up: &mut bool,
    last_line_at: &mut i64,
) -> SessionEnd {
    let stream = tokio::select! {
        r = connect(&cfg.host, cfg.port, resolver_slot) => match r {
            Ok(s) => s,
            Err(e) => return SessionEnd::Lost(Lost::Connect(e)),
        },
        _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
    };
    let (mut rd, mut wr) = stream.into_split();

    // Tolerant login read on raw bytes, so a prompt with no newline is
    // seen. Pre-login bytes are banner/prompt text and are discarded.
    let mut unterminated: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::select! {
            r = tokio::time::timeout(LOGIN_PROMPT_WAIT, rd.read(&mut chunk)) => r,
            _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
        };
        match read {
            Err(_silence) => break,
            Ok(Ok(0)) => return SessionEnd::Lost(Lost::ClosedBeforeLogin),
            Ok(Ok(n)) => {
                let bytes = &chunk[..n];
                if bytes.contains(&b'\n') {
                    break;
                }
                unterminated.extend_from_slice(bytes);
                if ends_like_a_prompt(&unterminated) {
                    break;
                }
                if unterminated.len() > MAX_LINE_BYTES {
                    return SessionEnd::Lost(Lost::PromptTooLong);
                }
            }
            Ok(Err(e)) => return SessionEnd::Lost(Lost::Read(e)),
        }
    }

    // The one and only write of the session.
    let login = format!("{}\r\n", cfg.login_callsign);
    tokio::select! {
        r = tokio::time::timeout(LOGIN_WRITE_TIMEOUT, wr.write_all(login.as_bytes())) => match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return SessionEnd::Lost(Lost::LoginWrite(e.to_string())),
            Err(_) => return SessionEnd::Lost(Lost::LoginWrite("timed out".to_string())),
        },
        _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
    }
    // `wr` stays alive (unused) until the session ends: dropping an
    // `OwnedWriteHalf` shuts down the write side, and a half-closed
    // client may be disconnected by the relay.
    let _wr = wr;

    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    let mut non_cw: u64 = 0;
    let mut deadline = Instant::now() + FEED_IDLE_TIMEOUT;
    loop {
        // `read_line_bounded` is resumable: `line` is not cleared until a
        // whole line is in, so losing a race never drops consumed bytes.
        let read = tokio::select! {
            r = read_line_bounded(&mut reader, &mut line) => r,
            _ = tokio::time::sleep_until(deadline) => return SessionEnd::Lost(Lost::Idle),
            _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
        };
        match read {
            Ok(0) => {
                tracing::debug!(
                    "rbn_compare: feed session ended; {non_cw} non-CW spot lines skipped"
                );
                return SessionEnd::Lost(Lost::Closed);
            }
            Ok(_) => {}
            Err(e) => return SessionEnd::Lost(Lost::Read(e)),
        }
        // One clock read per line: `Up`, the report and a later `Down` all
        // carry the same arrival time.
        *last_line_at = clock();
        match ak1a::classify(&line) {
            LineKind::NotSpot => {}
            kind => {
                if !*up {
                    *up = true;
                    metrics.set_rbn_feed_connected(true);
                    sink(FeedEvent::Up { at: *last_line_at });
                    tracing::info!(
                        "rbn_compare: feed connected, logged in as {}, receiving spots",
                        cfg.login_callsign
                    );
                }
                match kind {
                    LineKind::Cw(dx) => {
                        metrics.inc_rbn_feed_reports();
                        sink(FeedEvent::Report(RbnReport {
                            spotter: dx.spotter,
                            call: dx.call,
                            freq_hz: dx.freq_khz * 1000.0,
                            wpm: dx.wpm.map(f64::from),
                            time: *last_line_at,
                        }));
                    }
                    _ => non_cw += 1,
                }
            }
        }
        line.clear();
        deadline = Instant::now() + FEED_IDLE_TIMEOUT;
    }
}

/// The unterminated buffer ends in `:` plus optional spaces: a prompt
/// such as `Please enter your call: ` waiting for an answer.
fn ends_like_a_prompt(buf: &[u8]) -> bool {
    let trimmed = buf
        .iter()
        .rposition(|&b| b != b' ')
        .map(|i| &buf[..=i])
        .unwrap_or(&[]);
    trimmed.last() == Some(&b':')
}

/// Resolves (unless `host` is an IP literal) and connects with
/// `uplink.rs`'s per-address and overall connect bounds.
async fn connect(
    host: &str,
    port: u16,
    resolver_slot: &Arc<Semaphore>,
) -> std::io::Result<TcpStream> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => resolve(host, port, resolver_slot).await?,
    };
    connect_first_reachable_bounded(addrs, CONNECT_TIMEOUT, OVERALL_CONNECT_TIMEOUT, |addr| {
        TcpStream::connect(addr)
    })
    .await
}

/// `uplink.rs`'s bounded DNS lookup: the permit is held inside the spawned
/// lookup until the blocking `getaddrinfo` finishes, so a lookup this
/// future abandoned on timeout still blocks the next one.
async fn resolve(
    host: &str,
    port: u16,
    resolver_slot: &Arc<Semaphore>,
) -> std::io::Result<Vec<SocketAddr>> {
    let Ok(permit) = resolver_slot.clone().try_acquire_owned() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("a previous DNS lookup for {host}:{port} is still outstanding"),
        ));
    };
    let owned_host = host.to_string();
    let lookup = tokio::task::spawn(async move {
        let _permit = permit;
        tokio::net::lookup_host((owned_host.as_str(), port))
            .await
            .map(|it| it.collect::<Vec<_>>())
    });
    match tokio::time::timeout(CONNECT_TIMEOUT, lookup).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(std::io::Error::other(format!(
            "DNS lookup task for {host}:{port} failed: {join_err}"
        ))),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("resolving {host}:{port} timed out"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_colon_with_optional_spaces_is_a_prompt() {
        assert!(ends_like_a_prompt(b"Please enter your call:"));
        assert!(ends_like_a_prompt(b"Please enter your call: "));
        assert!(ends_like_a_prompt(b"login:   "));
        assert!(!ends_like_a_prompt(b"Welcome"));
        assert!(!ends_like_a_prompt(b"   "));
        assert!(!ends_like_a_prompt(b""));
    }
}
