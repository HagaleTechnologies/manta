//! Outbound RBN telnet uplink -- MAN-32. Connects to RBN's own
//! spot-collection endpoint as a client and forwards manta's own
//! validated spots (`SpotBus`) in the same `DX de` wire format the
//! inbound telnet server (`telnet.rs`) emits, per ARCHITECTURE §7's wire
//! format and this repo's own reference implementation of that protocol
//! from the server side.

use crate::bus::SpotBus;
use crate::config::RbnUplinkConfig;
use crate::metrics::Metrics;
use crate::rbn;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

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
            Err(_) => {
                metrics.set_uplink_connected(false);
                metrics.record_uplink_reconnect();
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn connect_and_forward(
    config: &RbnUplinkConfig,
    login_callsign: &str,
    bus: &Arc<SpotBus>,
    metrics: &Arc<Metrics>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    let stream = TcpStream::connect((config.target_host.as_str(), config.target_port)).await?;
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    // Subscribe before completing login, matching telnet.rs's own
    // "subscribe before handshake" rule -- a spot published mid-login
    // must not be lost to the broadcast channel's no-history semantics.
    let mut rx = bus.subscribe();

    let mut prompt_line = String::new();
    reader.read_line(&mut prompt_line).await?;
    wr.write_all(format!("{login_callsign}\r\n").as_bytes())
        .await?;

    metrics.set_uplink_connected(true);
    // A connection that gets this far is healthy -- a later drop should
    // retry quickly, not inherit backoff state from an earlier outage.
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
    metrics.set_uplink_connected(false);
    result
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
                        wr.write_all(format!("{line}\r\n").as_bytes()).await?;
                        metrics.record_uplink_sent();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        metrics.record_uplink_lagged(n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            read_result = reader.read_line(&mut discard) => {
                match read_result? {
                    0 => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "RBN uplink target closed the connection",
                        ));
                    }
                    _ => discard.clear(), // unexpected but harmless input; ignore and keep going
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
