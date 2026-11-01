//! MAN-320: the skimmer relay worker (D3, D5, D9, D10). Logs in to another
//! local CW skimmer's telnet port (CW Skimmer, SkimSrv or a second manta)
//! and publishes the CW spots that skimmer made under `callsign` on the
//! bus's relay channel, which only the RBN uplink reads (D4).
//!
//! Connection handling is `rbn_compare/feed.rs`'s: the bounded connect with
//! a resolver slot, the tolerant login read that answers a newline-less
//! `Please enter your callsign: ` prompt, and the `backoff.rs` 1 s -> 60 s
//! ladder. Unlike the RBN feed there is no idle timeout -- a skimmer on a
//! dead band can be silent for hours -- so a blank CRLF line goes out every
//! [`KEEPALIVE_INTERVAL`] to notice a vanished peer; a failed or timed-out
//! keepalive write ends the session. No `SKIMMER/SETT`, no password.

use crate::backoff::{next_backoff, AttemptOutcome, INITIAL_BACKOFF};
use crate::bounded_io::{read_line_bounded, MAX_LINE_BYTES};
use crate::bus::SpotBus;
use crate::metrics::{DropReason, RelayStats};
use crate::rbn_compare::feed::{connect, ends_like_a_prompt, shutdown_signalled};
use crate::rbn_compare::Clock;
use crate::relay::skimmer_line::{self, RelayLine};
use crate::relay::{plausible_relayed_call, RelayMode, RelaySpot};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Semaphore};

/// One `[[skimmer_relay]]` block, as the worker needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SkimmerRelay {
    pub host: String,
    pub port: u16,
    /// The login, and the only `DX de` spotter forwarded (D3).
    pub callsign: String,
}

/// D10: how often a blank CRLF keepalive goes to the skimmer.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(300);
/// How long the session may stay silent after connecting before the login
/// goes out anyway (a server that prompts with nothing at all).
pub const LOGIN_PROMPT_WAIT: Duration = Duration::from_secs(10);
/// Bounds the login and every keepalive write (D10).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

enum SessionEnd {
    Shutdown,
    /// Lost; `true` once the login went out (the 1 s retry rung).
    Lost {
        logged_in: bool,
        why: String,
    },
}

/// Runs one skimmer relay until shutdown (or its sender is dropped).
/// Reconnects forever otherwise. Keepalives go out every
/// [`KEEPALIVE_INTERVAL`].
pub async fn run(
    cfg: SkimmerRelay,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    shutdown: watch::Receiver<bool>,
) {
    run_with_keepalive(cfg, KEEPALIVE_INTERVAL, clock, bus, stats, shutdown).await;
}

/// [`run`] with the keepalive cadence as a parameter. Production uses
/// [`run`]; the tests use a sub-second cadence so they can check it in
/// real time over real loopback TCP (MAN-478: paused tokio time and real
/// sockets race on macOS). `keepalive_every` must be non-zero.
pub async fn run_with_keepalive(
    cfg: SkimmerRelay,
    keepalive_every: Duration,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let label = format!("{}:{}", cfg.host, cfg.port);
    tracing::info!(
        "skimmer_relay {label}: connecting; spots this skimmer makes as {} go out through \
         [[rbn_uplink]]",
        cfg.callsign
    );
    let resolver_slot = Arc::new(Semaphore::new(1));
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let end = session(
            &cfg,
            keepalive_every,
            &label,
            &clock,
            &bus,
            &stats,
            &mut shutdown,
            &resolver_slot,
        )
        .await;
        stats.set_up(false);
        let (logged_in, why) = match end {
            SessionEnd::Shutdown => return,
            SessionEnd::Lost { logged_in, why } => (logged_in, why),
        };
        let outcome = if logged_in {
            AttemptOutcome::Disconnected
        } else {
            AttemptOutcome::NeverConnected
        };
        let sleep_for = match outcome {
            AttemptOutcome::Disconnected => INITIAL_BACKOFF,
            AttemptOutcome::NeverConnected => backoff,
        };
        backoff = next_backoff(sleep_for, &outcome);
        if logged_in {
            tracing::warn!(
                "skimmer_relay {label}: connection lost ({why}); reconnecting in {} s",
                sleep_for.as_secs()
            );
        } else {
            tracing::warn!(
                "skimmer_relay {label}: cannot connect ({why}); retrying in {} s",
                sleep_for.as_secs()
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = shutdown_signalled(&mut shutdown) => return,
        }
    }
}

fn lost(logged_in: bool, why: impl Into<String>) -> SessionEnd {
    SessionEnd::Lost {
        logged_in,
        why: why.into(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    cfg: &SkimmerRelay,
    keepalive_every: Duration,
    label: &str,
    clock: &Clock,
    bus: &SpotBus,
    stats: &RelayStats,
    shutdown: &mut watch::Receiver<bool>,
    resolver_slot: &Arc<Semaphore>,
) -> SessionEnd {
    let stream = tokio::select! {
        r = connect(&cfg.host, cfg.port, resolver_slot) => match r {
            Ok(s) => s,
            Err(e) => return lost(false, format!("connect failed: {e}")),
        },
        _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
    };
    let (mut rd, mut wr) = stream.into_split();

    // `feed.rs`'s tolerant login read: raw bytes, so a prompt with no
    // newline is seen. Pre-login bytes are banner/prompt text, discarded.
    let mut unterminated: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::select! {
            r = tokio::time::timeout(LOGIN_PROMPT_WAIT, rd.read(&mut chunk)) => r,
            _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
        };
        match read {
            Err(_silence) => break,
            Ok(Ok(0)) => return lost(false, "closed before the login prompt"),
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
                    return lost(
                        false,
                        format!("login prompt longer than {MAX_LINE_BYTES} bytes with no line end"),
                    );
                }
            }
            Ok(Err(e)) => return lost(false, format!("read error: {e}")),
        }
    }

    let login = format!("{}\r\n", cfg.callsign);
    tokio::select! {
        r = tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(login.as_bytes())) => match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return lost(false, format!("login write failed: {e}")),
            Err(_) => return lost(false, "login write failed: timed out"),
        },
        _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
    }
    stats.set_up(true);
    tracing::info!("skimmer_relay {label}: logged in as {}", cfg.callsign);

    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    let mut warned_other_spotter = false;
    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + keepalive_every,
        keepalive_every,
    );
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // `read_line_bounded` is resumable: `line` is not cleared until a
        // whole line is in, so losing a race never drops consumed bytes.
        // No idle timeout (D10).
        let read = tokio::select! {
            r = read_line_bounded(&mut reader, &mut line) => r,
            _ = keepalive.tick() => {
                match tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(b"\r\n")).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => return lost(true, format!("keepalive write failed: {e}")),
                    Err(_) => return lost(true, "keepalive write failed: timed out"),
                }
            }
            _ = shutdown_signalled(shutdown) => return SessionEnd::Shutdown,
        };
        match read {
            Ok(0) => return lost(true, "connection closed by the skimmer"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return lost(true, format!("bad line from the skimmer: {e}"))
            }
            Err(e) => return lost(true, format!("read error: {e}")),
        }
        relay_line(
            &line,
            cfg,
            label,
            clock,
            bus,
            stats,
            &mut warned_other_spotter,
        );
        line.clear();
    }
}

/// One line from the skimmer: classify, apply D3/D5, publish.
fn relay_line(
    line: &str,
    cfg: &SkimmerRelay,
    label: &str,
    clock: &Clock,
    bus: &SpotBus,
    stats: &RelayStats,
    warned_other_spotter: &mut bool,
) {
    let spot = match skimmer_line::parse(line) {
        RelayLine::NotSpot => return,
        RelayLine::NotCw => {
            stats.record_dropped(DropReason::NotCw);
            return;
        }
        RelayLine::Cw(spot) => spot,
    };
    if spot.spotter != cfg.callsign {
        stats.record_dropped(DropReason::OtherSpotter);
        if !*warned_other_spotter {
            *warned_other_spotter = true;
            tracing::warn!(
                "skimmer_relay {label}: skipping spots made by {} -- this relay forwards only \
                 {}'s own spots; set callsign to the call the skimmer spots under",
                spot.spotter,
                cfg.callsign
            );
        }
        return;
    }
    if !plausible_relayed_call(&spot.call) {
        stats.record_dropped(DropReason::BadCall);
        return;
    }
    // Counted only when an uplink connection received it (see wsjtx.rs).
    if bus.publish_relay(RelaySpot {
        spotter: Arc::from(cfg.callsign.as_str()),
        callsign: spot.call,
        freq_hz: spot.freq_khz * 1000.0,
        mode: RelayMode::Cw { wpm: spot.wpm },
        snr_db: spot.snr_db,
        spot_type: spot.spot_type,
        unix_ts: clock(),
    }) {
        stats.record_spot();
    }
}
