//! MAN-320: the WSJT-X relay worker (placeholder; replaced below).

use crate::bus::SpotBus;
use crate::metrics::RelayStats;
use crate::rbn_compare::Clock;
use std::sync::Arc;
use tokio::sync::watch;

/// One `[[wsjtx_relay]]` block, as the worker needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct WsjtxRelay {
    /// The `relay` label and log prefix: the bound address.
    pub label: String,
    /// The spotter call every forwarded spot goes out under.
    pub callsign: String,
}

/// Runs one WSJT-X relay over a pre-bound socket until shutdown.
pub async fn run(
    cfg: WsjtxRelay,
    socket: std::net::UdpSocket,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let _ = (cfg, socket, clock, bus, stats);
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}
