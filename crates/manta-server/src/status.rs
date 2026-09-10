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

/// Scenario 1's one-line banner. The verb is `listening:`, not `ready:`
/// (MAN-122 review round 2): at the point every field here is known the
/// daemon has bound its three sockets and nothing more -- the decode
/// pipeline has not initialized, and a replay shorter than
/// `manta_engine`'s two-second calibration window (or a live source that
/// fails its first reads) still exits without ever decoding a sample.
/// Readiness to decode is a separate, later event; see
/// `format_pipeline_ready`.
pub fn format_startup_banner(info: &StartupInfo<'_>) -> String {
    format!(
        "manta {} listening: source={} sample_rate_hz={:.0} dial_freq_hz={:.0} station={} \
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

/// The readiness event proper: emitted once, by the daemon wiring layer,
/// when the decode pipeline has actually initialized (channelizer built,
/// calibration window read, first batch processed) -- the point after
/// which a bound socket is backed by a running decoder. Split from
/// `format_startup_banner` in MAN-122 review round 2, because binding
/// sockets is not evidence that anything will ever be decoded.
pub fn format_pipeline_ready(version: &str, source: &str, sample_rate_hz: f64) -> String {
    format!("manta {version} ready: decoding source={source} sample_rate_hz={sample_rate_hz:.0}")
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

/// What the status line's `pipeline=` field reports, derived from
/// `Metrics::pipeline_batches()` moving (or not) between two consecutive
/// status samples (MAN-122 review round 2). Without this the status line
/// happily republishes a stale `tracks=N` forever after the synchronous
/// decode loop stops progressing -- exactly the "is it still decoding?"
/// question the line exists to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    /// No batch has been processed yet, and the daemon is still inside
    /// `STARTUP_GRACE`. Expected for the first interval or two of a live
    /// source, whose `manta_engine` calibration window is two real seconds
    /// of samples; NOT reported as a stall, which would be a false alarm on
    /// every cold start.
    Starting,
    Decoding,
    /// Batches were being processed, and then stopped, without the daemon
    /// shutting down -- or none was ever processed and `STARTUP_GRACE` has
    /// since elapsed.
    Stalled,
}

/// How long zero pipeline progress is still reported as `starting` rather
/// than `stalled` (MAN-122 review round 3).
///
/// Without a bound, a daemon whose very first `IqSource::read` blocks
/// forever -- an SDR that accepts the connection and then never streams, a
/// KiwiSDR whose audio channel is silently dropped -- leaves
/// `pipeline_batches` at zero for the life of the process, so every status
/// line it ever emits says `pipeline=starting`. The liveness signal would
/// then report a wedged startup as a healthy cold start, indefinitely.
///
/// 30 s is ~15x the work a healthy cold start actually has to do here: the
/// source is already open before the status task is spawned (`main.rs`
/// constructs the `IqSource` before `start_spot_server`), so all that is
/// left is `manta_engine`'s two-second calibration read, the channelizer
/// build, and the filter-length padding batch. It is also under the 60 s
/// `DEFAULT_STATUS_INTERVAL`, so at the default cadence a wedged startup is
/// reported as `stalled` on the very first status line rather than never.
pub const STARTUP_GRACE: Duration = Duration::from_secs(30);

impl PipelineState {
    fn as_str(self) -> &'static str {
        match self {
            PipelineState::Starting => "starting",
            PipelineState::Decoding => "decoding",
            PipelineState::Stalled => "stalled",
        }
    }

    /// Classify from two consecutive samples of `Metrics::pipeline_batches()`
    /// plus how long the daemon has been up.
    ///
    /// `since_start` is what stops "no batch yet" from being a permanent
    /// excuse: past `STARTUP_GRACE` a pipeline that has still never
    /// processed a batch is wedged, not starting, and says so.
    pub fn from_progress(previous: u64, current: u64, since_start: Duration) -> Self {
        if current == 0 {
            if since_start >= STARTUP_GRACE {
                PipelineState::Stalled
            } else {
                PipelineState::Starting
            }
        } else if current == previous {
            PipelineState::Stalled
        } else {
            PipelineState::Decoding
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
    pub pipeline: PipelineState,
}

pub fn format_status_line(s: &StatusSnapshot) -> String {
    format!(
        "manta status: uptime_s={} pipeline={} tracks={} spots_per_min={:.1} spots_total={} \
         clients={} (telnet={} json={} ws={}) uplink={}",
        s.uptime_s,
        s.pipeline.as_str(),
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
        let mut last_batches = metrics.pipeline_batches();
        loop {
            // `biased`: without it `select!` picks a ready branch at random,
            // so on the poll where the sleep and the shutdown notification
            // both come ready the sleep can win and this task emits one more
            // status line into the shutdown drain (MAN-122 review round 2).
            // Shutdown first makes that unrepresentable.
            tokio::select! {
                biased;
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
                _ = tokio::time::sleep(interval) => {}
            }
            // Second guard, for a shutdown that lands between the sleep
            // completing and this line: `biased` orders the two futures
            // within one poll, it does not order them against the wall
            // clock. Cheap (one `watch` borrow) and it closes the window
            // completely.
            if *shutdown.borrow() {
                return;
            }
            let now = Instant::now();
            let uptime = now.duration_since(started);
            let spots_total = metrics.spots_total();
            let batches = metrics.pipeline_batches();
            let snapshot = StatusSnapshot {
                uptime_s: uptime.as_secs(),
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
                pipeline: PipelineState::from_progress(last_batches, batches, uptime),
            };
            last_sample = now;
            last_spots = spots_total;
            last_batches = batches;
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
            pipeline: PipelineState::Decoding,
        });
        for needle in [
            "pipeline=decoding",
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

    /// MAN-122 review round 2: a wedged decode loop (a blocked
    /// `IqSource::read`, say) leaves every other field in the snapshot
    /// frozen at its last live value, so `pipeline=` is the only thing that
    /// can tell an operator the difference between "quiet band" and "not
    /// running".
    #[test]
    fn the_pipeline_field_separates_a_stalled_loop_from_a_running_one() {
        let young = Duration::ZERO;
        assert_eq!(
            PipelineState::from_progress(0, 0, young),
            PipelineState::Starting
        );
        assert_eq!(
            PipelineState::from_progress(0, 7, young),
            PipelineState::Decoding
        );
        assert_eq!(
            PipelineState::from_progress(7, 12, young),
            PipelineState::Decoding
        );
        // Batches were flowing, and then stopped: the gauge below still says
        // `tracks=N`, but nothing is being decoded.
        assert_eq!(
            PipelineState::from_progress(12, 12, young),
            PipelineState::Stalled
        );
    }

    /// MAN-122 review round 3: zero progress is only `starting` for a
    /// bounded grace period. A first `IqSource::read` that blocks forever
    /// never bumps `pipeline_batches`, and before this the daemon reported
    /// `pipeline=starting` for as long as it stayed wedged -- the one
    /// classification an operator reads as "fine, give it a moment".
    #[test]
    fn a_startup_that_never_produces_a_batch_becomes_stalled_once_grace_elapses() {
        // Inside the window: still a plausible cold start.
        assert_eq!(
            PipelineState::from_progress(0, 0, STARTUP_GRACE - Duration::from_secs(1)),
            PipelineState::Starting
        );
        // At and past it: wedged, and the status line has to say so.
        assert_eq!(
            PipelineState::from_progress(0, 0, STARTUP_GRACE),
            PipelineState::Stalled
        );
        assert_eq!(
            PipelineState::from_progress(0, 0, Duration::from_secs(3600)),
            PipelineState::Stalled
        );
        // Grace never *demotes* a pipeline that did start: a daemon up for
        // an hour whose batches are still advancing is decoding.
        assert_eq!(
            PipelineState::from_progress(900, 1000, Duration::from_secs(3600)),
            PipelineState::Decoding
        );
        // The grace period must be shorter than the default cadence, or the
        // default deployment's first status line could never report it.
        assert!(STARTUP_GRACE < DEFAULT_STATUS_INTERVAL);
    }

    /// The two events are distinct on purpose (MAN-122 review round 2): the
    /// banner names bound sockets, which is not evidence the pipeline will
    /// ever start.
    #[test]
    fn the_banner_claims_only_that_the_listeners_are_bound() {
        let banner = format_startup_banner(&StartupInfo {
            version: "0.1.0",
            source: "file",
            sample_rate_hz: 48_000.0,
            dial_freq_hz: 14_060_000.0,
            station_callsign: "W5AU",
            telnet_addr: "127.0.0.1:7300".parse().unwrap(),
            json_addr: "127.0.0.1:7301".parse().unwrap(),
            metrics_addr: "127.0.0.1:7302".parse().unwrap(),
        });
        assert!(banner.contains("listening:"), "{banner:?}");
        assert!(!banner.contains("ready:"), "{banner:?}");
        let ready = format_pipeline_ready("0.1.0", "file", 48_000.0);
        assert!(ready.contains("ready: decoding"), "{ready:?}");
        assert!(ready.contains("source=file"), "{ready:?}");
    }

    /// A status line must never land after shutdown has been signalled --
    /// it would interleave into the shutdown drain and claim liveness the
    /// daemon no longer has. The one-poll tie between the sleep and the
    /// notification is not constructible from a test (the sender wakes the
    /// task before the paused clock advances), so it is closed structurally
    /// instead, by `biased` plus the post-sleep `shutdown.borrow()` guard;
    /// what this test pins is the observable half -- the task returns on
    /// shutdown, promptly, of its own accord, rather than surviving to the
    /// next tick.
    #[tokio::test(start_paused = true)]
    async fn the_status_task_returns_on_shutdown_instead_of_ticking_again() {
        let (tx, rx) = watch::channel(false);
        let metrics = Arc::new(Metrics::new());
        let handle = spawn_status_line(metrics, Duration::from_secs(1), 0, rx)
            .expect("a non-zero interval must spawn the task");
        tokio::time::sleep(Duration::from_millis(999)).await;
        tx.send(true).unwrap();
        // The task must return of its own accord rather than being aborted.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the status task must exit on shutdown, not outlive it")
            .expect("the status task must not panic");
    }
}
