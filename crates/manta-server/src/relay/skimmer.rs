//! MAN-320: the skimmer relay worker (placeholder; replaced below).

use crate::bus::SpotBus;
use crate::metrics::RelayStats;
use crate::rbn_compare::Clock;
use std::sync::Arc;
use tokio::sync::watch;

/// One `[[skimmer_relay]]` block, as the worker needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SkimmerRelay {
    pub host: String,
    pub port: u16,
    /// The login, and the only `DX de` spotter forwarded (D3).
    pub callsign: String,
}

/// Runs one skimmer relay until shutdown.
pub async fn run(
    cfg: SkimmerRelay,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let _ = (cfg, clock, bus, stats);
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}
