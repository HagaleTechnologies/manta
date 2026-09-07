//! MAN-122: operator-liveness logging. A startup banner (Scenario 1) and a
//! rate-limited periodic status line (Scenario 2) so an operator can tell
//! from the daemon's own log output that it came up and is decoding,
//! without waiting for a client to connect. Follows the plain-message,
//! `RUST_LOG`-gated, stderr-only `tracing` conventions MAN-59 established
//! (`docs/DECISIONS/2026-09-03-man59-connection-audit-logging.md`).

use crate::metrics::Metrics;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Default periodic status-line cadence when `status_interval_secs` is
/// omitted from `[server]`.
pub const DEFAULT_STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// Everything Scenario 1's startup banner names.
pub struct StartupInfo<'a> {
    pub version: &'a str,
    pub source: &'a str,
    pub sample_rate_hz: f64,
    pub dial_freq_hz: f64,
    pub station_callsign: &'a str,
    pub telnet_addr: SocketAddr,
    pub json_addr: SocketAddr,
    pub metrics_addr: SocketAddr,
}

pub fn format_startup_banner(info: &StartupInfo<'_>) -> String {
    format!(
        "manta {} ready: source={} sample_rate_hz={:.0} dial_freq_hz={:.0} station={} \
         telnet={} json={} metrics={}",
        info.version,
        info.source,
        info.sample_rate_hz,
        info.dial_freq_hz,
        info.station_callsign,
        info.telnet_addr,
        info.json_addr,
        info.metrics_addr,
    )
}

/// What the status line's `uplink=` field reports. `NotConfigured` is
/// distinct from `Disconnected` on purpose: an operator with no
/// `[[rbn_uplink]]` must not read a permanent "disconnected" as a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkState {
    NotConfigured,
    Connected,
    Disconnected,
}

impl UplinkState {
    fn as_str(self) -> &'static str {
        match self {
            UplinkState::NotConfigured => "none",
            UplinkState::Connected => "connected",
            UplinkState::Disconnected => "disconnected",
        }
    }
}

pub struct StatusSnapshot {
    pub uptime_s: u64,
    pub active_tracks: u64,
    pub spots_per_min: f64,
    pub spots_total: u64,
    pub telnet_clients: i64,
    pub json_clients: i64,
    pub ws_clients: i64,
    pub uplink: UplinkState,
}

pub fn format_status_line(s: &StatusSnapshot) -> String {
    format!(
        "manta status: uptime_s={} tracks={} spots_per_min={:.1} spots_total={} \
         clients={} (telnet={} json={} ws={}) uplink={}",
        s.uptime_s,
        s.active_tracks,
        s.spots_per_min,
        s.spots_total,
        s.telnet_clients + s.json_clients + s.ws_clients,
        s.telnet_clients,
        s.json_clients,
        s.ws_clients,
        s.uplink.as_str(),
    )
}

/// Spots per minute over the elapsed window. Returns 0.0 for a zero window
/// rather than an `inf`/`NaN` an operator would have to decode.
pub fn spots_per_min(delta_spots: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    delta_spots as f64 * 60.0 / secs
}

/// Spawns the periodic status-line task. `interval` of zero disables it
/// (returns `None`, nothing is spawned). Follows the codebase's existing
/// periodic-task idiom (`tasks::spawn_reaper`, `rate_limit::
/// spawn_stale_entry_reaper`): a bare sleep loop, not `tokio::time::
/// interval` (unused anywhere in this workspace), but additionally raced
/// against the shutdown watch so the task cannot emit a status line into
/// the middle of the shutdown drain.
pub fn spawn_status_line(
    metrics: Arc<Metrics>,
    interval: Duration,
    uplink_targets: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    if interval.is_zero() {
        return None;
    }
    Some(tokio::spawn(async move {
        let started = Instant::now();
        let mut last_sample = started;
        let mut last_spots = metrics.spots_total();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = shutdown.changed() => {
                    // `Err` means every `watch::Sender` has dropped -- there is no
                    // shutdown signal left to observe, ever. Reading the stale
                    // borrowed value and `continue`-ing here (as an earlier
                    // revision did) would re-fire this arm instantly on every
                    // future `select!` iteration, busy-spinning a core on a
                    // project whose hard budget is one Raspberry Pi 4 core.
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                    continue;
                }
            }
            let now = Instant::now();
            let spots_total = metrics.spots_total();
            let snapshot = StatusSnapshot {
                uptime_s: now.duration_since(started).as_secs(),
                active_tracks: metrics.active_tracks(),
                spots_per_min: spots_per_min(
                    spots_total.saturating_sub(last_spots),
                    now.duration_since(last_sample),
                ),
                spots_total,
                telnet_clients: metrics.telnet_clients(),
                json_clients: metrics.json_clients(),
                ws_clients: metrics.ws_clients(),
                uplink: if uplink_targets == 0 {
                    UplinkState::NotConfigured
                } else if metrics.uplink_connected() {
                    UplinkState::Connected
                } else {
                    UplinkState::Disconnected
                },
            };
            last_sample = now;
            last_spots = spots_total;
            tracing::info!("{}", format_status_line(&snapshot));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_names_every_field_scenario_1_requires() {
        let line = format_startup_banner(&StartupInfo {
            version: "0.1.0",
            source: "file",
            sample_rate_hz: 48_000.0,
            dial_freq_hz: 14_060_000.0,
            station_callsign: "W5AU",
            telnet_addr: "127.0.0.1:7300".parse().unwrap(),
            json_addr: "127.0.0.1:7301".parse().unwrap(),
            metrics_addr: "127.0.0.1:7302".parse().unwrap(),
        });
        for needle in [
            "0.1.0",
            "source=file",
            "sample_rate_hz=48000",
            "dial_freq_hz=14060000",
            "telnet=127.0.0.1:7300",
            "json=127.0.0.1:7301",
            "metrics=127.0.0.1:7302",
        ] {
            assert!(line.contains(needle), "{needle:?} missing from {line:?}");
        }
    }

    #[test]
    fn the_status_line_names_every_field_scenario_2_requires() {
        let line = format_status_line(&StatusSnapshot {
            uptime_s: 120,
            active_tracks: 3,
            spots_per_min: 12.0,
            spots_total: 24,
            telnet_clients: 1,
            json_clients: 2,
            ws_clients: 0,
            uplink: UplinkState::Connected,
        });
        for needle in [
            "tracks=3",
            "spots_per_min=12.0",
            "clients=3",
            "uplink=connected",
        ] {
            assert!(line.contains(needle), "{needle:?} missing from {line:?}");
        }
    }

    #[test]
    fn spots_per_min_scales_the_window_and_never_divides_by_zero() {
        assert_eq!(spots_per_min(10, Duration::from_secs(60)), 10.0);
        assert_eq!(spots_per_min(10, Duration::from_secs(30)), 20.0);
        assert_eq!(spots_per_min(0, Duration::ZERO), 0.0);
        assert_eq!(spots_per_min(5, Duration::ZERO), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_interval_disables_the_status_task() {
        let (_tx, rx) = watch::channel(false);
        assert!(spawn_status_line(Arc::new(Metrics::new()), Duration::ZERO, 0, rx).is_none());
    }
}
