//! Prometheus text-format metrics. ARCHITECTURE §8: "manta --status hits a
//! local control socket... Prometheus text endpoint... spot rate, active
//! tracks..."; MAN-12 scenario 3 ("operators can inspect health without
//! reading source"). `Metrics` owns what `manta-server` genuinely knows
//! (spots published, connected clients per protocol) and exposes
//! `set_active_tracks`/`set_source_health`/`set_input_health` for the
//! daemon wiring layer to inject engine-owned numbers manta-server has no
//! way to compute itself. `set_source_health` also feeds the per-source
//! outage counters (`manta_source_outages_total`,
//! `manta_source_down_seconds_total`; MAN-96).

use crate::health::{DecodeWatch, HealthCheck, HealthReport};
use manta_spot::SpotType;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

/// Rolling window over which "recent" reconnects are counted (MAN-44).
/// Matches the ticket's "reconnecting repeatedly over several minutes".
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(300);
/// Recent reconnects at or above this count read as a stuck reconnect
/// loop. `backoff::MAX_BACKOFF` is 60s, so a target that is simply down
/// produces ~5 reconnects per window -- comfortably over this -- while a
/// single transient blip (whose backoff then resets to
/// `backoff::INITIAL_BACKOFF`) does not trip it.
pub const FLAPPING_RECONNECTS: u32 = 3;
/// Hard cap on retained reconnect timestamps per target. A target
/// flapping far faster than the window would otherwise grow this
/// unbounded; the cumulative reconnect counter stays exact regardless,
/// only `recent_reconnects` saturates. Same bounded-cardinality
/// discipline as MAN-62's `OccurrenceTracker` (see `bus.rs`).
const MAX_TRACKED_RECONNECTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkHealth {
    Disabled,
    Connected,
    /// Reconnecting repeatedly -- `recent_reconnects >= FLAPPING_RECONNECTS`.
    Flapping,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverallUplinkHealth {
    Ok,
    Degraded,
    Down,
    Disabled,
}

/// Priority order is deliberate (MAN-44 decision 5): `Flapping` outranks
/// `Connected` because a target reconnecting every 60s is momentarily
/// connected whenever you happen to look, and reporting that as healthy
/// is exactly the failure this ticket exists to prevent.
fn classify_uplink_health(enabled: bool, connected: bool, recent_reconnects: u32) -> UplinkHealth {
    if !enabled {
        UplinkHealth::Disabled
    } else if recent_reconnects >= FLAPPING_RECONNECTS {
        UplinkHealth::Flapping
    } else if connected {
        UplinkHealth::Connected
    } else {
        UplinkHealth::Down
    }
}

/// Pure function over an already-taken snapshot, so a caller that also
/// needs per-target rows (`status_doc::StatusDoc::from_metrics`) derives
/// both from the SAME snapshot at the SAME `now` (MAN-44 code review
/// CR-1/CR-3).
pub(crate) fn overall_uplink_health_of(snapshot: &[UplinkTargetSnapshot]) -> OverallUplinkHealth {
    let enabled: Vec<&UplinkTargetSnapshot> = snapshot.iter().filter(|t| t.enabled).collect();
    if enabled.is_empty() {
        return OverallUplinkHealth::Disabled;
    }
    let connected = enabled
        .iter()
        .filter(|t| t.health == UplinkHealth::Connected)
        .count();
    if connected == enabled.len() {
        OverallUplinkHealth::Ok
    } else if connected == 0 {
        OverallUplinkHealth::Down
    } else {
        OverallUplinkHealth::Degraded
    }
}

fn prune_reconnects(recent: &mut VecDeque<Instant>, now: Instant) {
    while let Some(front) = recent.front() {
        // saturating: a synthetic `now` earlier than an entry (possible
        // only from a test driving `_at` with out-of-order instants) must
        // not panic.
        if now.saturating_duration_since(*front) > RECONNECT_WINDOW {
            recent.pop_front();
        } else {
            break;
        }
    }
}

/// One target's point-in-time state, as `GET /status` reports it
/// (MAN-44). Taken by `UplinkTarget::snapshot_at` so a whole registry is
/// read against one consistent `now`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UplinkTargetSnapshot {
    #[serde(rename = "target")]
    pub label: String,
    pub enabled: bool,
    pub connected: bool,
    pub sent: u64,
    pub suppressed: u64,
    pub lagged: u64,
    pub write_failed: u64,
    pub disconnected: u64,
    pub reconnects: u64,
    pub recent_reconnects: u32,
    pub health: UplinkHealth,
}

/// Spots abandoned when a client connection terminates on a failed write,
/// or (via `Metrics::record_dropped_shutdown`) on a clean shutdown with no
/// write having failed or timed out (an earlier handshake write may well
/// have succeeded -- see `spots_dropped_shutdown_total`). `in_flight_spot`
/// is true when the write that just failed was carrying a NEWLY-LOST live
/// spot (that spot is lost too) and false for a control-frame write --
/// telnet's `Filter set:` acknowledgement, the WebSocket Pong reply --
/// where nothing was in flight but the receiver's queue is abandoned all
/// the same.
/// `still_queued` is `rx.len()`.
///
/// MAN-45 remediate (code-review round 18, finding 1): telnet's `sh/dx`
/// history-replay write-failure site also passes `false`, even though a
/// write did fail there -- the entry it was writing is a REPLAY of a spot
/// already published (and already counted once in `manta_spots_total`,
/// often already delivered live to this same client), not newly-lost live
/// data, so neither it nor the rest of the unreplayed history iterator is
/// summed into `still_queued`. Only that call site's live `rx` backlog is
/// real, newly-abandoned loss.
///
/// One function rather than the arithmetic open-coded at each site: rounds
/// 11-16 each found one more write path with this accounting missing or
/// subtly different, and the differences between `1 + rx.len()` and bare
/// `rx.len()` are exactly what made each one easy to get wrong. Extracted
/// for the same reason `json_stream::is_ws_protocol_violation` was: so the
/// policy is testable without a live socket.
pub fn abandoned_spot_count(in_flight_spot: bool, still_queued: usize) -> u64 {
    still_queued as u64 + u64::from(in_flight_spot)
}

/// Packet-level input-stream health for one source, injected by the daemon
/// wiring layer (see module doc) -- `manta-server` has no `manta-input`
/// dependency and cannot read these itself (MAN-56). Deliberately a
/// separate type from `manta_input::InputHealthCounters` rather than a
/// shared one: keeping the two crates' types disjoint is what preserves
/// that dependency boundary; `manta-cli`, which depends on both, does the
/// translation.
///
/// The values are the source's current *absolute* monotonic totals, not
/// deltas -- `set_input_health` overwrites, it does not accumulate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputHealth {
    pub dropped_packets: u64,
    pub gaps_detected: u64,
    pub malformed_packets: u64,
}

/// Escapes a Prometheus label VALUE (not a label name, which is always one
/// of this codebase's own `&'static str`s). MAN-128: the uplink target
/// label is the first one in this codebase derived from operator-supplied
/// TOML rather than a fixed, compile-time set of names -- shared by the
/// (unmerged, at plan time) MAN-44 PR #95 so the two converge on one name.
pub(crate) fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// MAN-316: where the reference list in use came from -- the `origin`
/// label of `manta_list_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrigin {
    /// The copy compiled into manta.
    Bundled,
    /// `cty_path`/`scp_path` (or `--cty`/`--scp`).
    File,
    /// `[spot].list_cache_dir`'s last good download, read at startup.
    Cache,
    /// Downloaded by this run's refresher.
    Download,
}

impl ListOrigin {
    pub fn label(self) -> &'static str {
        match self {
            ListOrigin::Bundled => "bundled",
            ListOrigin::File => "file",
            ListOrigin::Cache => "cache",
            ListOrigin::Download => "download",
        }
    }
}

/// MAN-316: one reference-list download attempt's result -- the `outcome`
/// label of `manta_list_refresh_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRefreshOutcome {
    /// A new version passed the checks and is in use.
    Updated,
    /// 304 Not Modified, or the same bytes as the copy in use.
    Unchanged,
    /// A network or HTTP error, or a download the checks rejected.
    Failed,
}

impl ListRefreshOutcome {
    /// Render order of `manta_list_refresh_total`'s outcomes.
    const ALL: [ListRefreshOutcome; 3] = [
        ListRefreshOutcome::Updated,
        ListRefreshOutcome::Unchanged,
        ListRefreshOutcome::Failed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ListRefreshOutcome::Updated => "updated",
            ListRefreshOutcome::Unchanged => "unchanged",
            ListRefreshOutcome::Failed => "failed",
        }
    }

    fn index(self) -> usize {
        match self {
            ListRefreshOutcome::Updated => 0,
            ListRefreshOutcome::Unchanged => 1,
            ListRefreshOutcome::Failed => 2,
        }
    }
}

/// MAN-316: one reference list's series across the three `manta_list_*`
/// families. Each field is `None` until its setter first runs, so a list
/// the refresher does not manage publishes no refresh series rather than
/// a permanently-zero one.
#[derive(Debug, Default)]
struct ListMetrics {
    /// `(sha256, origin)` of the copy in use, for `manta_list_info`.
    in_use: Option<(String, ListOrigin)>,
    /// `manta_list_refresh_total`, indexed by `ListRefreshOutcome::index`;
    /// `Some` once `enable_list_refresh` (or a first attempt) ran.
    refresh_totals: Option<[u64; 3]>,
    /// `manta_list_last_success_timestamp_seconds`.
    last_success: Option<SystemTime>,
}

pub(crate) fn spot_type_metric_label(t: SpotType) -> &'static str {
    match t {
        SpotType::Cq => "cq",
        SpotType::De => "de",
        SpotType::Beacon => "beacon",
        SpotType::Unknown => "unknown",
    }
}

/// Build metadata, rendered as a Prometheus "info metric" (a gauge that is
/// always `1`, carrying the real payload as labels) -- MAN-128.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    pub version: String,
    pub git_sha: String,
    pub features: String,
}

/// A snapshot of `manta-engine`'s decode-latency observer, ready to render
/// as a Prometheus histogram (MAN-128). `bucket_counts` is
/// NON-cumulative (length = `bounds_seconds.len() + 1`, the last being the
/// `+Inf` overflow bucket) -- `render_prometheus_text` does the running
/// sum Prometheus's `_bucket{le=...}` convention requires.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyHistogram {
    pub bounds_seconds: Vec<f64>,
    pub bucket_counts: Vec<u64>,
    pub sum_seconds: f64,
}

impl LatencyHistogram {
    fn total_count(&self) -> u64 {
        self.bucket_counts.iter().sum()
    }
}

/// A unitless histogram snapshot (MAN-118), shaped like `LatencyHistogram`:
/// `bucket_counts` is NON-cumulative, length `bounds.len() + 1`, the last
/// being the `+Inf` overflow bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueHistogram {
    pub bounds: Vec<f64>,
    pub bucket_counts: Vec<u64>,
    pub sum: f64,
}

/// A snapshot of `manta-engine`'s decode-quality observer (MAN-118): the
/// four families that report whether the decode itself looks healthy, as
/// opposed to whether the daemon is running at all.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeQuality {
    pub chars_total: u64,
    pub garbles_total: u64,
    pub rail_ratio: ValueHistogram,
    pub repetitions: ValueHistogram,
}

/// Writes `h` as Prometheus histogram samples under `name`: cumulative
/// `_bucket{le=...}` lines, `+Inf`, `_sum` and `_count` (equal to `+Inf`).
fn push_histogram_samples(out: &mut String, name: &str, h: &ValueHistogram) {
    let mut cumulative = 0u64;
    for (bound, count) in h.bounds.iter().zip(h.bucket_counts.iter()) {
        cumulative += count;
        out.push_str(&format!("{name}_bucket{{le=\"{bound}\"}} {cumulative}\n"));
    }
    cumulative += h.bucket_counts.get(h.bounds.len()).copied().unwrap_or(0);
    out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {cumulative}\n"));
    out.push_str(&format!("{name}_sum {}\n", h.sum));
    out.push_str(&format!("{name}_count {cumulative}\n"));
}

/// Captured once, at `Metrics::new()` -- the moment the metrics endpoint
/// comes into existence. `instant` (monotonic) backs `manta_uptime_seconds`
/// so an NTP step never distorts it; `unix_seconds` (wall-clock) backs
/// `manta_start_time_seconds` for a human-readable timestamp. NOT seeded
/// from `resolve_epoch` -- that's the *recording's* mtime on file replay,
/// not this process's actual start time (MAN-128 plan, Key Discoveries).
struct StartedAt {
    instant: Instant,
    unix_seconds: f64,
}

impl Default for StartedAt {
    fn default() -> Self {
        Self {
            instant: Instant::now(),
            unix_seconds: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        }
    }
}

/// One configured RBN uplink target's own counters (MAN-128 D6). Replaces
/// the pre-MAN-128 shared atomics: deriving the aggregate families by
/// summing over a `Vec<Arc<UplinkTarget>>` removes the MAN-42 class of
/// desync bug structurally (a target can only ever affect its OWN
/// `connected` flag) rather than by convention (the old
/// `mark_uplink_connected`/`mark_uplink_disconnected` pairing discipline).
pub struct UplinkTarget {
    label: String,
    enabled: bool,
    connected: AtomicBool,
    sent: AtomicU64,
    suppressed: AtomicU64,
    ceiling_suppressed: AtomicU64,
    lagged: AtomicU64,
    write_failed: AtomicU64,
    disconnected: AtomicU64,
    reconnects: AtomicU64,
    /// Reconnect instants inside `RECONNECT_WINDOW` (MAN-44), bounded by
    /// `MAX_TRACKED_RECONNECTS`.
    recent: Mutex<VecDeque<Instant>>,
}

impl UplinkTarget {
    fn new(label: String, enabled: bool) -> Self {
        Self {
            label,
            enabled,
            connected: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            ceiling_suppressed: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            write_failed: AtomicU64::new(0),
            disconnected: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn record_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// A spot this target received but deliberately did not send: either
    /// `dry_run` is on, or its type is outside the target's `spot_types`
    /// (MAN-91).
    pub fn record_suppressed(&self) {
        self.suppressed.fetch_add(1, Ordering::Relaxed);
    }

    /// A spot this target would otherwise have written, skipped because
    /// the target's `max_spots_per_minute` ceiling was reached (MAN-248).
    /// Deliberately separate from `record_suppressed`: that counter's
    /// HELP text and METRICS.md meaning promise "dry-run or spot_types",
    /// and the ticket requires a distinguishable suppressed-by-ceiling
    /// counter so an operator can tell a runaway input or a too-low
    /// default from ordinary filtering.
    pub fn record_ceiling_suppressed(&self) {
        self.ceiling_suppressed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_lagged(&self, n: u64) {
        self.lagged.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_write_failed(&self, n: u64) {
        self.write_failed.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_disconnected(&self, n: u64) {
        self.disconnected.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_reconnect(&self) {
        self.record_reconnect_at(Instant::now());
    }

    /// `_at` variant so the MAN-44 reconnect window is testable without
    /// sleeping; `record_reconnect` above is the only production caller.
    pub fn record_reconnect_at(&self, now: Instant) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
        let mut recent = self
            .recent
            .lock()
            .expect("uplink recent-reconnects lock poisoned");
        prune_reconnects(&mut recent, now);
        if recent.len() == MAX_TRACKED_RECONNECTS {
            recent.pop_front();
        }
        recent.push_back(now);
    }

    #[cfg(test)]
    fn recent_len(&self) -> usize {
        self.recent
            .lock()
            .expect("uplink recent-reconnects lock poisoned")
            .len()
    }

    /// Reconnects inside the trailing `RECONNECT_WINDOW` as of `now`.
    pub fn recent_reconnects_at(&self, now: Instant) -> u32 {
        let mut recent = self
            .recent
            .lock()
            .expect("uplink recent-reconnects lock poisoned");
        prune_reconnects(&mut recent, now);
        recent.len() as u32
    }

    pub fn snapshot_at(&self, now: Instant) -> UplinkTargetSnapshot {
        let recent_reconnects = self.recent_reconnects_at(now);
        let connected = self.connected();
        UplinkTargetSnapshot {
            label: self.label.clone(),
            enabled: self.enabled,
            connected,
            sent: self.sent_total(),
            suppressed: self.suppressed_total(),
            lagged: self.lagged_total(),
            write_failed: self.write_failed_total(),
            disconnected: self.disconnected_total(),
            reconnects: self.reconnects_total(),
            recent_reconnects,
            health: classify_uplink_health(self.enabled, connected, recent_reconnects),
        }
    }

    /// Call exactly once per connect transition -- unlike the pre-MAN-128
    /// shared counter, this is a plain per-target flag: another target's
    /// calls can never touch it.
    pub fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
    }

    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn sent_total(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn suppressed_total(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    pub fn ceiling_suppressed_total(&self) -> u64 {
        self.ceiling_suppressed.load(Ordering::Relaxed)
    }

    pub fn lagged_total(&self) -> u64 {
        self.lagged.load(Ordering::Relaxed)
    }

    pub fn write_failed_total(&self) -> u64 {
        self.write_failed.load(Ordering::Relaxed)
    }

    pub fn disconnected_total(&self) -> u64 {
        self.disconnected.load(Ordering::Relaxed)
    }

    pub fn reconnects_total(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }
}

/// MAN-96: one source's outage history. An outage is a healthy -> unhealthy
/// edge; an initial unhealthy report (HPSDR starts unhealthy until
/// confirmed live) is start-up, not an outage. `down_seconds_total` only
/// grows when an outage ENDS, so an outage still in progress shows only as
/// `manta_source_health == 0`.
#[derive(Debug, Default)]
struct SourceOutages {
    outages_total: u64,
    down_seconds_total: f64,
    down_since: Option<Instant>,
}

/// MAN-64 round 7 (V-2): a source's health value, plus whether that value
/// is final. Added when a MAN-55 liveness watcher task could race the
/// fatal-exit write in `record_terminal_source_health`
/// (`manta-cli/src/main.rs`) and flip the gauge back to healthy after it.
/// MAN-73 has since removed that task: apart from file replay's one-time
/// startup write, every regular write now comes from `ReconnectingSource`'s
/// health sink, called synchronously inside a `read()` on the decode
/// thread. `listen` has returned (and dropped the source) before the
/// fatal-exit write runs, so no current writer can land after it. The
/// marker stays so the fatal write remains final against any later
/// asynchronous writer: see `set_source_health_terminal`.
#[derive(Clone, Copy)]
struct SourceHealthEntry {
    healthy: bool,
    terminal: bool,
}

#[derive(Default)]
pub struct Metrics {
    spots_total: AtomicU64,
    spots_dropped_lagged_total: AtomicU64,
    spots_suppressed_by_filter_total: AtomicU64,
    spots_dropped_write_failed_total: AtomicU64,
    /// MAN-45 remediate (code-review round 18, finding 2): a client's
    /// subscribed backlog abandoned because the daemon shut down while
    /// that client was still in telnet's pre-login handshake or
    /// json_stream's pre-loop WS-detection peek/handshake -- NO write
    /// itself timed out or failed on this path, the daemon shut down
    /// cleanly. Kept out of `spots_dropped_write_failed_total` for the
    /// same reason `uplink_disconnected_total` is kept out of
    /// `uplink_write_failed_total`: that counter's own name and
    /// Prometheus HELP text ("a client's socket write timed out or
    /// failed") would otherwise mislead an operator into diagnosing a
    /// failing client socket when the real cause was a normal shutdown.
    spots_dropped_shutdown_total: AtomicU64,
    /// MAN-45 remediate (code-review round 19, P2): history entries
    /// abandoned mid-`sh/dx` replay -- the entry whose write just failed
    /// plus every entry left unreplayed behind it, and the unreplayed
    /// remainder when the replay loop defers to the shutdown drain.
    ///
    /// A DEDICATED counter (one of the two remedies the reviewer offered)
    /// rather than more `record_write_failed` calls, for the same reason
    /// `spots_dropped_shutdown_total` is separate: a replayed `sh/dx`
    /// entry is a RE-send of a spot already published and already counted
    /// once in `manta_spots_total` -- often already delivered live to this
    /// same client before `sh/dx` was even issued. Summing it into
    /// `spots_dropped_write_failed_total` would let that counter exceed
    /// `manta_spots_total` and would break the "delivered + counted ==
    /// published" invariant the acceptance suite asserts on it. Keeping it
    /// here makes the loss VISIBLE (ARCHITECTURE §8: no silent loss)
    /// without corrupting the live-delivery arithmetic.
    spots_replay_abandoned_total: AtomicU64,
    /// In-window spots evicted by the history ceiling (MAN-93).
    spots_history_truncated_total: AtomicU64,
    /// MAN-136/MAN-45: a spot's dx or de callsign couldn't be resolved
    /// against `cty.dat` -- or resolved only through the base prefix of a
    /// `/MM`/`/AM` call, whose real position is unknowable from it -- so it
    /// was emitted with the `UNKNOWN_DXCC`/`UNKNOWN_CONTINENT`/
    /// `UNKNOWN_CQ_ZONE` sentinels instead of real geography. ARCHITECTURE §8: "every dropped/evicted/suppressed item is
    /// counted" -- an operator otherwise has no way to notice this is
    /// happening. Incremented once per spot at publish time (`main.rs`), not
    /// inside `SpotMessage::from_spot` (which runs once per connected
    /// client).
    spots_unresolved_geography_total: AtomicU64,
    /// MAN-128: `(band, spot_type)` counter, additive to
    /// `spots_total` -- a NEW family, not a replacement, so an existing
    /// `sum()` over the unlabeled series is unaffected. Bounded cardinality:
    /// the fixed `band::BANDS_HZ` table (plus `"unknown"`) times the four
    /// `SpotType` variants.
    spots_by_band: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    telnet_clients: AtomicI64,
    json_clients: AtomicI64,
    ws_clients: AtomicI64,
    active_tracks: AtomicU64,
    /// MAN-312: static crashes the engine has detected, injected by the
    /// daemon wiring layer like `active_tracks`.
    static_crashes_total: AtomicU64,
    /// MAN-314: `manta_rtty_signals{band}` -- RTTY signals the engine's
    /// detector currently marks, by amateur band. A band's series appears
    /// at its first mark and stays (at 0) after its signals clear; bounded
    /// by the fixed `band::BANDS_HZ` table plus `"unknown"`.
    rtty_by_band: Mutex<BTreeMap<&'static str, u64>>,
    /// Monotonic count of batches the decode pipeline has processed, bumped
    /// once per pipeline batch by the daemon wiring layer (MAN-122 review
    /// round 2). Its VALUE is meaningless to an operator; what matters is
    /// that it keeps moving -- a status line that reports `tracks=N` off a
    /// stale gauge while the synchronous decode loop is wedged (a blocked
    /// `IqSource::read` after an audio device stops delivering callbacks,
    /// say) claims the daemon is decoding when it is not. Deliberately not
    /// exported in the Prometheus text: it is a liveness edge, not a figure
    /// worth graphing.
    pipeline_batches: AtomicU64,
    source_health: RwLock<BTreeMap<String, SourceHealthEntry>>,
    /// MAN-96: per-source outage bookkeeping behind
    /// `manta_source_outages_total` / `manta_source_down_seconds_total`.
    /// Updated under `source_health`'s write guard (the only place both
    /// locks are taken, always in that order).
    source_outages: Mutex<BTreeMap<String, SourceOutages>>,
    /// MAN-56: HPSDR's (and any future source's) packet loss/malformed
    /// counters. Engine/input-owned figures, injected by the daemon wiring
    /// layer on a timer -- see `manta-cli`'s `INPUT_HEALTH_POLL_INTERVAL`.
    /// Sources with no wire-packet loss model never call `set_input_health`,
    /// so they publish no series rather than a permanently-zero one.
    input_health: RwLock<BTreeMap<String, InputHealth>>,
    /// MAN-316: per reference list (`cty`, `scp`), fed by `manta-cli`'s
    /// startup resolution and list refresher. Ordered so `/metrics`
    /// renders the lists in a stable order.
    lists: Mutex<BTreeMap<&'static str, ListMetrics>>,
    /// MAN-128: one handle per configured `[[rbn_uplink]]` target,
    /// registered in config order before `uplink::serve` is spawned for it
    /// (so a never-connected or disabled target still renders). The seven
    /// pre-MAN-128 aggregate series are now DERIVED by summing over this
    /// registry rather than tracked as separate shared atomics.
    uplink_targets: RwLock<Vec<Arc<UplinkTarget>>>,
    /// MAN-128: captured once at construction -- see `StartedAt`'s doc
    /// comment for why this is not `#[derive(Default)]`-trivial and not
    /// seeded from `resolve_epoch`.
    started: StartedAt,
    build_info: RwLock<Option<BuildInfo>>,
    decode_latency: RwLock<Option<LatencyHistogram>>,
    /// MAN-118: the decode-quality families' latest snapshot; `None` (no
    /// decode loop yet) renders their headers only.
    decode_quality: RwLock<Option<DecodeQuality>>,
    /// MAN-128: which listeners (telnet/json/metrics) are currently up, per
    /// `/healthz`'s "listeners are up" check and the `manta_listener_up`
    /// gauge.
    listener_up: RwLock<BTreeMap<String, bool>>,
    /// MAN-128: the decode loop's own liveness, as last observed by the
    /// daemon wiring layer -- see `crate::health::DecodeWatch`.
    decode_watch: Mutex<DecodeWatch>,
    /// MAN-301: set once by `enable_rbn_compare`; `None` renders no
    /// `manta_rbn_compare_*` family at all.
    rbn_compare: OnceLock<RbnCompareMetrics>,
    /// MAN-324: set once by `enable_live`; `None` renders no
    /// `manta_live_*` family at all.
    live: OnceLock<LiveMetrics>,
}

/// MAN-324: the live-state stream's families, kept apart from the spot
/// families so `manta_spots_dropped_*` still counts only spots.
#[derive(Default)]
struct LiveMetrics {
    clients: AtomicI64,
    /// Per `crate::live_state::LIVE_RECORD_KINDS` entry.
    records: [AtomicU64; 4],
    dropped_lagged: AtomicU64,
    dropped_write_failed: AtomicU64,
    dropped_shutdown: AtomicU64,
}

/// MAN-324: the live-state stream's metrics. Every method is a no-op until
/// `enable_live` has been called.
impl Metrics {
    pub fn enable_live(&self) {
        let _ = self.live.set(LiveMetrics::default());
    }

    /// One record of `kind` published to at least one subscriber.
    pub fn record_live_published(&self, kind: &str) {
        let Some(live) = self.live.get() else {
            return;
        };
        if let Some(i) = crate::live_state::LIVE_RECORD_KINDS
            .iter()
            .position(|k| *k == kind)
        {
            live.records[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A live client fell `n` records behind and was disconnected.
    pub fn record_live_lagged(&self, n: u64) {
        if let Some(live) = self.live.get() {
            live.dropped_lagged.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// A live client's write failed or timed out with `n` records unsent.
    pub fn record_live_write_failed(&self, n: u64) {
        if let Some(live) = self.live.get() {
            live.dropped_write_failed.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// `n` records were still unsent to a live client at shutdown.
    pub fn record_live_dropped_shutdown(&self, n: u64) {
        if let Some(live) = self.live.get() {
            live.dropped_shutdown.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn inc_live_clients(&self) {
        if let Some(live) = self.live.get() {
            live.clients.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dec_live_clients(&self) {
        if let Some(live) = self.live.get() {
            live.clients.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Live clients connected now; 0 when the live stream is off.
    pub fn live_clients(&self) -> i64 {
        self.live
            .get()
            .map_or(0, |l| l.clients.load(Ordering::Relaxed))
    }

    /// Live records lost to disconnected clients, all reasons.
    pub fn live_records_dropped(&self) -> u64 {
        self.live.get().map_or(0, |l| {
            l.dropped_lagged.load(Ordering::Relaxed)
                + l.dropped_write_failed.load(Ordering::Relaxed)
                + l.dropped_shutdown.load(Ordering::Relaxed)
        })
    }

    fn render_live(&self, out: &mut String) {
        let Some(live) = self.live.get() else {
            return;
        };
        out.push_str(
            "# HELP manta_live_clients_connected Live-state stream clients connected now, raw TCP and WebSocket together.\n",
        );
        out.push_str("# TYPE manta_live_clients_connected gauge\n");
        out.push_str(&format!(
            "manta_live_clients_connected {}\n",
            live.clients.load(Ordering::Relaxed)
        ));
        out.push_str(
            "# HELP manta_live_records_total Live-state records published to at least one subscriber, by kind.\n",
        );
        out.push_str("# TYPE manta_live_records_total counter\n");
        for (kind, count) in crate::live_state::LIVE_RECORD_KINDS
            .iter()
            .zip(&live.records)
        {
            out.push_str(&format!(
                "manta_live_records_total{{kind=\"{kind}\"}} {}\n",
                count.load(Ordering::Relaxed)
            ));
        }
        out.push_str(
            "# HELP manta_live_records_dropped_total Live-state records a disconnected client never received, by reason.\n",
        );
        out.push_str("# TYPE manta_live_records_dropped_total counter\n");
        for (reason, count) in [
            ("lagged", &live.dropped_lagged),
            ("write_failed", &live.dropped_write_failed),
            ("shutdown", &live.dropped_shutdown),
        ] {
            out.push_str(&format!(
                "manta_live_records_dropped_total{{reason=\"{reason}\"}} {}\n",
                count.load(Ordering::Relaxed)
            ));
        }
    }
}

/// MAN-301: the trailing window `manta_rbn_compare_*_last_hour` gauges and
/// the per-band median frequency offset cover, measured from each grade's
/// emission time.
pub const GRADE_WINDOW: Duration = Duration::from_secs(3600);
/// Hard cap on retained grade timestamps per `(band, grade)`,
/// `(band, kind)` or per-band offset key. A node grading far faster than
/// the window would otherwise grow these unbounded; the cumulative
/// `manta_rbn_compare_grades_total` counter stays exact regardless, only
/// the last-hour gauges saturate. 20,000 per key is well over any real
/// band's hourly spot rate. Same bounded-cardinality discipline as
/// `MAX_TRACKED_RECONNECTS` and MAN-62's `OccurrenceTracker`.
const MAX_RECENT_GRADES: usize = 20_000;

fn grade_label(g: crate::rbn_compare::grade::Grade) -> &'static str {
    use crate::rbn_compare::grade::Grade;
    match g {
        Grade::Match => "match",
        Grade::Disagree => "disagree",
        Grade::Elsewhere => "elsewhere",
        Grade::None => "none",
        Grade::Unknown => "unknown",
    }
}

fn disagreement_label(d: crate::rbn_compare::grade::Disagreement) -> &'static str {
    use crate::rbn_compare::grade::Disagreement;
    match d {
        Disagreement::Frequency => "frequency",
        Disagreement::Speed => "speed",
    }
}

fn push_bounded<T>(ring: &mut VecDeque<T>, item: T) {
    if ring.len() == MAX_RECENT_GRADES {
        ring.pop_front();
    }
    ring.push_back(item);
}

fn grade_expired(at: Instant, now: Instant) -> bool {
    // saturating: a synthetic `now` earlier than an entry (only from a
    // test driving `_at` out of order) must not panic.
    now.saturating_duration_since(at) > GRADE_WINDOW
}

/// MAN-301 per-grade bookkeeping, one lock for all of it so a scrape sees
/// one consistent set.
#[derive(Default)]
struct RbnGradeBook {
    total: BTreeMap<(&'static str, &'static str), u64>,
    recent: BTreeMap<(&'static str, &'static str), VecDeque<Instant>>,
    disagreements: BTreeMap<(&'static str, &'static str), VecDeque<Instant>>,
    offsets: BTreeMap<&'static str, VecDeque<(Instant, i64)>>,
}

impl RbnGradeBook {
    fn prune(&mut self, now: Instant) {
        for ring in self
            .recent
            .values_mut()
            .chain(self.disagreements.values_mut())
        {
            while ring.front().is_some_and(|at| grade_expired(*at, now)) {
                ring.pop_front();
            }
        }
        for ring in self.offsets.values_mut() {
            while ring.front().is_some_and(|(at, _)| grade_expired(*at, now)) {
                ring.pop_front();
            }
        }
    }
}

/// MAN-301: the `manta_rbn_compare_*` families. Present on `Metrics` only
/// once `enable_rbn_compare` has run, so a node without `[rbn_compare]`
/// renders none of them.
#[derive(Default)]
struct RbnCompareMetrics {
    grades: Mutex<RbnGradeBook>,
    feed_connected: AtomicBool,
    feed_reports_total: AtomicU64,
    feed_reconnects_total: AtomicU64,
    pending: AtomicU64,
    ungraded_total: AtomicU64,
}

fn median_of(values: &mut [i64]) -> f64 {
    values.sort_unstable();
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2] as f64
    } else {
        (values[n / 2 - 1] as f64 + values[n / 2] as f64) / 2.0
    }
}

/// MAN-301: the live comparator's metrics. Every method is a no-op until
/// `enable_rbn_compare` has been called.
impl Metrics {
    pub fn enable_rbn_compare(&self) {
        let _ = self.rbn_compare.set(RbnCompareMetrics::default());
    }

    pub fn record_rbn_grade(&self, grade: &crate::rbn_compare::grade::SpotGrade) {
        self.record_rbn_grade_at(grade, Instant::now());
    }

    pub fn record_rbn_grade_at(&self, grade: &crate::rbn_compare::grade::SpotGrade, now: Instant) {
        use crate::rbn_compare::grade::Grade;
        let Some(rc) = self.rbn_compare.get() else {
            return;
        };
        let band = grade.band;
        let label = grade_label(grade.grade);
        let mut book = rc.grades.lock().expect("rbn_compare grades lock poisoned");
        *book.total.entry((band, label)).or_insert(0) += 1;
        push_bounded(book.recent.entry((band, label)).or_default(), now);
        if grade.grade == Grade::Disagree {
            let mut kinds: Vec<&'static str> = grade
                .disagreements
                .iter()
                .map(|d| disagreement_label(*d))
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            for kind in kinds {
                push_bounded(book.disagreements.entry((band, kind)).or_default(), now);
            }
        }
        if matches!(grade.grade, Grade::Match | Grade::Disagree) {
            if let Some(offset) = grade.freq_offset_hz {
                push_bounded(book.offsets.entry(band).or_default(), (now, offset));
            }
        }
    }

    pub fn set_rbn_feed_connected(&self, connected: bool) {
        if let Some(rc) = self.rbn_compare.get() {
            rc.feed_connected.store(connected, Ordering::Relaxed);
        }
    }

    pub fn inc_rbn_feed_reports(&self) {
        if let Some(rc) = self.rbn_compare.get() {
            rc.feed_reports_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn inc_rbn_feed_reconnects(&self) {
        if let Some(rc) = self.rbn_compare.get() {
            rc.feed_reconnects_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn set_rbn_pending(&self, pending: usize) {
        if let Some(rc) = self.rbn_compare.get() {
            rc.pending.store(pending as u64, Ordering::Relaxed);
        }
    }

    pub fn add_rbn_ungraded(&self, n: u64) {
        if let Some(rc) = self.rbn_compare.get() {
            rc.ungraded_total.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn render_rbn_compare_at(&self, out: &mut String, now: Instant) {
        let Some(rc) = self.rbn_compare.get() else {
            return;
        };
        let mut book = rc.grades.lock().expect("rbn_compare grades lock poisoned");
        book.prune(now);

        out.push_str(
            "# HELP manta_rbn_compare_grades_total Spots graded against the RBN feed since start, by the spot's band and grade.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_grades_total counter\n");
        for ((band, grade), count) in book.total.iter() {
            out.push_str(&format!(
                "manta_rbn_compare_grades_total{{band=\"{band}\",grade=\"{grade}\"}} {count}\n"
            ));
        }

        out.push_str(&format!(
            "# HELP manta_rbn_compare_grades_last_hour Spots graded against the RBN feed in the last {} s, by the spot's band and grade.\n",
            GRADE_WINDOW.as_secs()
        ));
        out.push_str("# TYPE manta_rbn_compare_grades_last_hour gauge\n");
        for ((band, grade), ring) in book.recent.iter() {
            out.push_str(&format!(
                "manta_rbn_compare_grades_last_hour{{band=\"{band}\",grade=\"{grade}\"}} {}\n",
                ring.len()
            ));
        }

        out.push_str(&format!(
            "# HELP manta_rbn_compare_disagreements_last_hour Spots graded disagree in the last {} s, by band and the tolerance that failed (frequency or speed); a spot failing both counts under each.\n",
            GRADE_WINDOW.as_secs()
        ));
        out.push_str("# TYPE manta_rbn_compare_disagreements_last_hour gauge\n");
        for ((band, kind), ring) in book.disagreements.iter() {
            out.push_str(&format!(
                "manta_rbn_compare_disagreements_last_hour{{band=\"{band}\",kind=\"{kind}\"}} {}\n",
                ring.len()
            ));
        }

        let medians: Vec<(&'static str, f64, usize)> = book
            .offsets
            .iter()
            .filter(|(_, ring)| !ring.is_empty())
            .map(|(band, ring)| {
                let mut values: Vec<i64> = ring.iter().map(|(_, o)| *o).collect();
                (*band, median_of(&mut values), values.len())
            })
            .collect();
        drop(book);

        out.push_str(&format!(
            "# HELP manta_rbn_compare_freq_offset_median_hz Median of manta's frequency minus RBN's modal frequency, in Hz, over spots graded match or disagree in the last {} s.\n",
            GRADE_WINDOW.as_secs()
        ));
        out.push_str("# TYPE manta_rbn_compare_freq_offset_median_hz gauge\n");
        for (band, median, _) in medians.iter() {
            out.push_str(&format!(
                "manta_rbn_compare_freq_offset_median_hz{{band=\"{band}\"}} {median}\n"
            ));
        }

        out.push_str(
            "# HELP manta_rbn_compare_freq_offset_samples Spots the median frequency offset is computed over.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_freq_offset_samples gauge\n");
        for (band, _, samples) in medians.iter() {
            out.push_str(&format!(
                "manta_rbn_compare_freq_offset_samples{{band=\"{band}\"}} {samples}\n"
            ));
        }

        out.push_str(
            "# HELP manta_rbn_compare_feed_connected Whether the RBN feed is logged in and delivering lines.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_feed_connected gauge\n");
        out.push_str(&format!(
            "manta_rbn_compare_feed_connected {}\n",
            if rc.feed_connected.load(Ordering::Relaxed) {
                1
            } else {
                0
            }
        ));

        out.push_str(
            "# HELP manta_rbn_compare_feed_reports_total CW reports accepted from the RBN feed.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_feed_reports_total counter\n");
        out.push_str(&format!(
            "manta_rbn_compare_feed_reports_total {}\n",
            rc.feed_reports_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_rbn_compare_feed_reconnects_total RBN feed connections lost or failed.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_feed_reconnects_total counter\n");
        out.push_str(&format!(
            "manta_rbn_compare_feed_reconnects_total {}\n",
            rc.feed_reconnects_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_rbn_compare_pending Spots waiting for their RBN comparison window to close.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_pending gauge\n");
        out.push_str(&format!(
            "manta_rbn_compare_pending {}\n",
            rc.pending.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_rbn_compare_ungraded_total Spots never graded: the pending queue was full, or the comparator lagged the spot bus.\n",
        );
        out.push_str("# TYPE manta_rbn_compare_ungraded_total counter\n");
        out.push_str(&format!(
            "manta_rbn_compare_ungraded_total {}\n",
            rc.ungraded_total.load(Ordering::Relaxed)
        ));
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// MAN-128: also breaks the count out by `(band, type)` into the
    /// `manta_spots_by_band_total` family -- see `spots_by_band`'s doc
    /// comment. `freq_hz.round()` matches `spot_message.rs`'s own rounding
    /// so a spot within 0.5 Hz of a band edge is never counted under a
    /// different band than the one the JSON stream reports.
    pub fn record_spot(&self, spot: &manta_spot::Spot) {
        self.spots_total.fetch_add(1, Ordering::Relaxed);
        let band = crate::band::band_for_freq_hz(spot.freq_hz.round());
        let kind = spot_type_metric_label(spot.spot_type);
        *self
            .spots_by_band
            .lock()
            .expect("spots_by_band lock poisoned")
            .entry((band, kind))
            .or_insert(0) += 1;
    }

    /// A subscriber fell behind and `n` spots it never saw were dropped
    /// (`broadcast::error::RecvError::Lagged(n)`) before it was
    /// disconnected. ARCHITECTURE §8: "every dropped/evicted/suppressed
    /// item is counted" -- this is what makes a lag-induced loss visible
    /// instead of silent.
    pub fn record_lagged(&self, n: u64) {
        self.spots_dropped_lagged_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// A spot was suppressed by a client's own `set dx filter unique > n`
    /// threshold, not by a broadcast-lag disconnect. ARCHITECTURE §8:
    /// "every dropped/evicted/suppressed item is counted" -- deliberate
    /// per-client filtering is exactly the kind of drop that would
    /// otherwise be silent (a quiet feed and heavy filtering look
    /// identical to an operator without this).
    pub fn record_filter_suppressed(&self, n: u64) {
        self.spots_suppressed_by_filter_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// A write to a client's socket timed out or failed before a
    /// `Lagged`/`Closed` broadcast error was ever observed (e.g. the
    /// client stopped reading but its TCP connection hasn't reset yet).
    /// `n` covers the spot whose write just failed plus whatever was
    /// still retained in the receiver's own buffer and is now abandoned
    /// along with it. Without this, a client lost this way shows zero
    /// loss on every counter -- ARCHITECTURE §8 requires every
    /// dropped/evicted/suppressed item counted, not just lag-induced loss
    /// (round-11 review finding).
    pub fn record_write_failed(&self, n: u64) {
        self.spots_dropped_write_failed_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Read accessor for the write-failure counter, mirroring the
    /// `uplink_*_total` getters -- lets an acceptance test assert the
    /// "delivered + counted == published" invariant (ARCHITECTURE §8)
    /// without parsing the Prometheus text body.
    pub fn spots_dropped_write_failed_total(&self) -> u64 {
        self.spots_dropped_write_failed_total
            .load(Ordering::Relaxed)
    }

    /// See `spots_dropped_shutdown_total`'s doc comment for why this is a
    /// separate counter from `record_write_failed` rather than another
    /// caller of it.
    pub fn record_dropped_shutdown(&self, n: u64) {
        self.spots_dropped_shutdown_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Read accessor mirroring `spots_dropped_write_failed_total` -- lets a
    /// test assert the two counters independently (e.g. that a clean
    /// shutdown mid-handshake charges this one and leaves the write-failure
    /// counter untouched).
    pub fn spots_dropped_shutdown_total(&self) -> u64 {
        self.spots_dropped_shutdown_total.load(Ordering::Relaxed)
    }

    /// `n` `sh/dx` history entries were abandoned without being replayed
    /// to the requesting client. See `spots_replay_abandoned_total`'s doc
    /// comment for why this is its own counter rather than another caller
    /// of `record_write_failed`.
    pub fn record_replay_abandoned(&self, n: u64) {
        self.spots_replay_abandoned_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Read accessor mirroring the other loss-counter getters, so an
    /// acceptance test can assert replay loss independently of live loss.
    pub fn spots_replay_abandoned_total(&self) -> u64 {
        self.spots_replay_abandoned_total.load(Ordering::Relaxed)
    }

    pub fn record_history_truncated(&self, n: u64) {
        self.spots_history_truncated_total
            .fetch_add(n, Ordering::Relaxed);
    }

    pub fn spots_history_truncated_total(&self) -> u64 {
        self.spots_history_truncated_total.load(Ordering::Relaxed)
    }

    /// MAN-136/MAN-45: call once per spot (not per connected client) when
    /// either the dx or de callsign didn't resolve against `cty.dat`, so the
    /// wire message carries the `UNKNOWN_*` sentinels instead of real
    /// geography.
    pub fn record_unresolved_geography(&self) {
        self.spots_unresolved_geography_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_telnet_clients(&self) {
        self.telnet_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_telnet_clients(&self) {
        self.telnet_clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_json_clients(&self) {
        self.json_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_json_clients(&self) {
        self.json_clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn inc_ws_clients(&self) {
        self.ws_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_ws_clients(&self) {
        self.ws_clients.fetch_sub(1, Ordering::Relaxed);
    }

    /// Engine-owned figure, injected by the daemon wiring layer (see
    /// module doc) -- `manta-server` has no track manager of its own.
    pub fn set_active_tracks(&self, count: u64) {
        self.active_tracks.store(count, Ordering::Relaxed);
    }

    /// MAN-312: the engine's running static-crash count. `fetch_max`, so a
    /// stale or out-of-order injection can never make the counter go
    /// backwards.
    pub fn set_static_crashes_total(&self, count: u64) {
        self.static_crashes_total
            .fetch_max(count, Ordering::Relaxed);
    }

    /// MAN-314: the engine's currently marked RTTY signals, by mark
    /// frequency in Hz. Replaces the previous set: every existing band
    /// series drops to 0 first, then each mark counts toward its band.
    /// `&[]` zeroes the family (outage, shutdown).
    pub fn set_rtty_signals(&self, marks_hz: &[f64]) {
        let mut by_band = self.rtty_by_band.lock().unwrap_or_else(|e| e.into_inner());
        by_band.values_mut().for_each(|v| *v = 0);
        for &hz in marks_hz {
            *by_band
                .entry(crate::band::band_for_freq_hz(hz.round()))
                .or_insert(0) += 1;
        }
    }

    /// Live-state getters for MAN-122's periodic status line. `Metrics` is
    /// already the daemon's one shared, synchronously-readable handle on live
    /// state (every subsystem holds an `Arc` clone of it), so the status task
    /// reads it directly instead of parsing `render_prometheus_text()` back
    /// out of a String.
    pub fn spots_total(&self) -> u64 {
        self.spots_total.load(Ordering::Relaxed)
    }

    pub fn active_tracks(&self) -> u64 {
        self.active_tracks.load(Ordering::Relaxed)
    }

    /// One decode batch finished. Called by the daemon wiring layer from
    /// `manta_engine::listen_with_observers`'s per-batch `on_tracks` observer -- see
    /// `pipeline_batches`.
    pub fn record_pipeline_batch(&self) {
        self.pipeline_batches.fetch_add(1, Ordering::Relaxed);
    }

    pub fn pipeline_batches(&self) -> u64 {
        self.pipeline_batches.load(Ordering::Relaxed)
    }

    pub fn telnet_clients(&self) -> i64 {
        self.telnet_clients.load(Ordering::Relaxed)
    }

    pub fn json_clients(&self) -> i64 {
        self.json_clients.load(Ordering::Relaxed)
    }

    pub fn ws_clients(&self) -> i64 {
        self.ws_clients.load(Ordering::Relaxed)
    }

    pub fn set_source_health(&self, source: &str, healthy: bool) {
        self.set_source_health_at(source, healthy, Instant::now());
    }

    /// MAN-96: `set_source_health` with an explicit observation time, so the
    /// outage counters are testable without sleeping. A healthy -> unhealthy
    /// edge counts one outage and starts its clock; the next unhealthy ->
    /// healthy edge adds the elapsed time to the completed down seconds.
    /// Repeated reports of the same state change nothing, and a write after
    /// a terminal one (MAN-64) changes neither the gauge nor the counters.
    pub fn set_source_health_at(&self, source: &str, healthy: bool, now: Instant) {
        self.write_source_health(source, healthy, false, now);
    }

    /// The definitive, final health value for `source`: once written, no
    /// later `set_source_health` call for the same source can change it.
    /// See `SourceHealthEntry`'s doc comment for why this is needed instead
    /// of always taking the most recent write. A healthy -> unhealthy
    /// terminal write still counts as an outage (MAN-96).
    pub fn set_source_health_terminal(&self, source: &str, healthy: bool) {
        self.write_source_health(source, healthy, true, Instant::now());
    }

    fn write_source_health(&self, source: &str, healthy: bool, terminal: bool, now: Instant) {
        let mut health = self
            .source_health
            .write()
            .expect("source_health lock poisoned");
        let prev = match health.get_mut(source) {
            // A terminal entry is this source's final word for the rest of
            // the process's life -- see `set_source_health_terminal`.
            Some(entry) if entry.terminal && !terminal => return,
            Some(entry) => {
                let prev = entry.healthy;
                *entry = SourceHealthEntry { healthy, terminal };
                Some(prev)
            }
            None => {
                health.insert(source.to_string(), SourceHealthEntry { healthy, terminal });
                None
            }
        };
        let mut outages = self
            .source_outages
            .lock()
            .expect("source_outages lock poisoned");
        let entry = outages.entry(source.to_string()).or_default();
        match (prev, healthy) {
            (Some(true), false) => {
                entry.outages_total += 1;
                entry.down_since = Some(now);
            }
            (Some(false), true) => {
                if let Some(since) = entry.down_since.take() {
                    entry.down_seconds_total += now.saturating_duration_since(since).as_secs_f64();
                }
            }
            _ => {}
        }
    }

    /// MAN-56: HPSDR's (and any future source's) packet loss/malformed
    /// counters. Engine/input-owned figures, injected by the daemon wiring
    /// layer on a timer -- see `main.rs`'s `INPUT_HEALTH_POLL_INTERVAL`.
    /// Sources with no wire-packet loss model never call this, so they
    /// publish no series rather than a permanently-zero one.
    pub fn set_input_health(&self, source: &str, health: InputHealth) {
        self.input_health
            .write()
            .expect("input_health lock poisoned")
            .insert(source.to_string(), health);
    }

    /// MAN-316: the reference lists' entries, tolerating a poisoned lock --
    /// a panic elsewhere must not take `/metrics` down with it.
    fn list_metrics(&self) -> std::sync::MutexGuard<'_, BTreeMap<&'static str, ListMetrics>> {
        self.lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// MAN-316: names the copy of `list` (`"cty"` or `"scp"`) every consumer
    /// uses now. Replaces the list's previous `manta_list_info` series, so a
    /// scrape never shows two versions of one list in use.
    pub fn set_list_in_use(&self, list: &'static str, sha256: &str, origin: ListOrigin) {
        self.list_metrics().entry(list).or_default().in_use = Some((sha256.to_string(), origin));
    }

    /// MAN-316: `list` is refreshed by this run, so `manta_list_refresh_total`
    /// publishes all three outcomes for it, starting at 0. Idempotent.
    pub fn enable_list_refresh(&self, list: &'static str) {
        self.list_metrics()
            .entry(list)
            .or_default()
            .refresh_totals
            .get_or_insert([0; 3]);
    }

    /// MAN-316: counts one download attempt for `list`. `Updated` and
    /// `Unchanged` both confirm the list in use is current, so they also
    /// move its last-success time to `at`.
    pub fn record_list_refresh(
        &self,
        list: &'static str,
        outcome: ListRefreshOutcome,
        at: SystemTime,
    ) {
        let mut lists = self.list_metrics();
        let entry = lists.entry(list).or_default();
        entry.refresh_totals.get_or_insert([0; 3])[outcome.index()] += 1;
        if outcome != ListRefreshOutcome::Failed {
            entry.last_success = Some(at);
        }
    }

    /// MAN-316: `list` was last confirmed current at `at` without an
    /// attempt in this run -- the cache's `checked_at`, read at startup.
    pub fn set_list_confirmed_at(&self, list: &'static str, at: SystemTime) {
        self.list_metrics().entry(list).or_default().last_success = Some(at);
    }

    // MAN-32/MAN-128: RBN uplink counters. ARCHITECTURE §8's "every
    // dropped/evicted/suppressed item is counted" invariant applies here
    // too -- a dry-run-suppressed or lag-dropped spot must be visible,
    // not silent. MAN-128: now a registry of per-target handles; the
    // aggregate getters below sum over it.

    /// Registers one `[[rbn_uplink]]` target and returns its handle, for
    /// `uplink::serve` to record against. Call BEFORE spawning `serve` for
    /// it, and for every configured target including disabled ones (D8):
    /// registration, not a live connection, is what makes a target appear
    /// on `/metrics` at all.
    pub fn register_uplink_target(&self, label: String, enabled: bool) -> Arc<UplinkTarget> {
        let target = Arc::new(UplinkTarget::new(label, enabled));
        self.uplink_targets
            .write()
            .expect("uplink_targets lock poisoned")
            .push(target.clone());
        target
    }

    fn uplink_targets_snapshot(&self) -> std::sync::RwLockReadGuard<'_, Vec<Arc<UplinkTarget>>> {
        self.uplink_targets
            .read()
            .expect("uplink_targets lock poisoned")
    }

    pub fn uplink_sent_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.sent_total())
            .sum()
    }

    pub fn uplink_suppressed_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.suppressed_total())
            .sum()
    }

    /// MAN-248: spots skipped because a target's forwarding ceiling was
    /// reached, summed over every configured target. Separate from
    /// `uplink_suppressed_total` for the same reason the per-target
    /// counter is (see `UplinkTarget::record_ceiling_suppressed`).
    pub fn uplink_ceiling_suppressed_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.ceiling_suppressed_total())
            .sum()
    }

    pub fn uplink_lagged_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.lagged_total())
            .sum()
    }

    pub fn uplink_write_failed_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.write_failed_total())
            .sum()
    }

    pub fn uplink_disconnected_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.disconnected_total())
            .sum()
    }

    pub fn uplink_reconnects_total(&self) -> u64 {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.reconnects_total())
            .sum()
    }

    /// Count of currently-connected targets -- unchanged name/semantics
    /// from the pre-MAN-128 gauge, now derived from the registry instead
    /// of a separately-tracked shared atomic.
    pub fn uplink_connected(&self) -> bool {
        self.uplink_targets_snapshot().iter().any(|t| t.connected())
    }

    fn uplink_connected_count(&self) -> i64 {
        self.uplink_targets_snapshot()
            .iter()
            .filter(|t| t.connected())
            .count() as i64
    }

    /// MAN-44: every registered target's state against one `now`, in
    /// config order (disabled targets included).
    pub fn uplink_snapshot_at(&self, now: Instant) -> Vec<UplinkTargetSnapshot> {
        self.uplink_targets_snapshot()
            .iter()
            .map(|t| t.snapshot_at(now))
            .collect()
    }

    pub fn uplink_snapshot(&self) -> Vec<UplinkTargetSnapshot> {
        self.uplink_snapshot_at(Instant::now())
    }

    /// `Ok` only when every ENABLED target's own health classifies as
    /// `Connected` (MAN-44 decision 5) -- a flapping target counts the
    /// same as a down one. `Disabled` when no target is enabled
    /// (including zero configured targets).
    pub fn uplink_overall_health(&self) -> OverallUplinkHealth {
        overall_uplink_health_of(&self.uplink_snapshot())
    }

    /// Time since this `Metrics` (and so the daemon's spot server)
    /// started -- the same monotonic origin as `manta_uptime_seconds`.
    pub fn uptime(&self) -> Duration {
        self.started.instant.elapsed()
    }

    // MAN-128: build-info, uptime, listener/decode health.

    pub fn set_build_info(&self, info: BuildInfo) {
        *self.build_info.write().expect("build_info lock poisoned") = Some(info);
    }

    pub fn set_listener_up(&self, listener: &str, up: bool) {
        self.listener_up
            .write()
            .expect("listener_up lock poisoned")
            .insert(listener.to_string(), up);
    }

    /// Arms the decode watchdog `/healthz`'s decode check reads -- call
    /// once, right after the daemon's listeners are up and before the
    /// decode loop starts. Library use that never arms this leaves the
    /// decode check reporting "not monitored" (and healthy), which is the
    /// correct state for non-daemon use of `Metrics`.
    pub fn arm_decode_watchdog(&self) {
        self.arm_decode_watchdog_at(Instant::now());
    }

    /// Test seam for `arm_decode_watchdog` with a synthetic `Instant`.
    #[doc(hidden)]
    pub fn arm_decode_watchdog_at(&self, now: Instant) {
        *self
            .decode_watch
            .lock()
            .expect("decode_watch lock poisoned") = DecodeWatch::Running {
            last_progress: now,
            last_count: 0,
        };
    }

    /// Marks the decode loop as having stopped -- makes `/healthz` report
    /// unhealthy regardless of how recently progress was observed. Call
    /// once `listen_with_observers` has returned, before the shutdown
    /// drain begins: that's exactly the window in which `source_health`
    /// alone would otherwise still read healthy for a source that already
    /// died.
    pub fn mark_decode_stopped(&self) {
        *self
            .decode_watch
            .lock()
            .expect("decode_watch lock poisoned") = DecodeWatch::Stopped;
    }

    pub fn set_decode_latency(&self, h: LatencyHistogram) {
        self.set_decode_latency_at(h, Instant::now());
    }

    /// Test seam for `set_decode_latency` with a synthetic `Instant`.
    #[doc(hidden)]
    pub fn set_decode_latency_at(&self, h: LatencyHistogram, now: Instant) {
        let total_count = h.total_count();
        *self
            .decode_latency
            .write()
            .expect("decode_latency lock poisoned") = Some(h);
        self.decode_watch
            .lock()
            .expect("decode_watch lock poisoned")
            .observe_progress(now, total_count);
    }

    /// MAN-118: replaces the decode-quality snapshot. Unlike
    /// `set_decode_latency` this does not feed the decode watchdog, which
    /// keeps keying off latency alone.
    pub fn set_decode_quality(&self, q: DecodeQuality) {
        *self
            .decode_quality
            .write()
            .expect("decode_quality lock poisoned") = Some(q);
    }

    /// Evaluates `/healthz`'s verdict (and `manta_healthy`/
    /// `manta_listener_up`'s values) at `now`. Healthy only when every
    /// registered source is healthy, every registered listener is up, AND
    /// the decode watchdog is either not armed or has seen progress within
    /// `health::DECODE_STALL_THRESHOLD` (MAN-128 D9). Takes its own locks
    /// and returns a value -- never call this while already holding a
    /// `Metrics` lock it also takes (see `render_prometheus_text`).
    pub fn health_at(&self, now: Instant) -> HealthReport {
        let mut checks = Vec::new();

        let sources = self
            .source_health
            .read()
            .expect("source_health lock poisoned");
        if sources.is_empty() {
            checks.push(HealthCheck {
                name: "source".to_string(),
                ok: false,
                detail: "source: none registered".to_string(),
            });
        } else {
            for (name, entry) in sources.iter() {
                checks.push(HealthCheck {
                    name: format!("source:{name}"),
                    ok: entry.healthy,
                    detail: format!(
                        "source {name}: {}",
                        if entry.healthy {
                            "healthy"
                        } else {
                            "unhealthy"
                        }
                    ),
                });
            }
        }
        drop(sources);

        let listeners = self.listener_up.read().expect("listener_up lock poisoned");
        if listeners.is_empty() {
            checks.push(HealthCheck {
                name: "listener".to_string(),
                ok: false,
                detail: "listener: none registered".to_string(),
            });
        } else {
            for (name, up) in listeners.iter() {
                checks.push(HealthCheck {
                    name: format!("listener:{name}"),
                    ok: *up,
                    detail: format!("listener {name}: {}", if *up { "up" } else { "down" }),
                });
            }
        }
        drop(listeners);

        checks.push(
            self.decode_watch
                .lock()
                .expect("decode_watch lock poisoned")
                .check(now),
        );

        let healthy = checks.iter().all(|c| c.ok);
        HealthReport { healthy, checks }
    }

    pub fn render_prometheus_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP manta_spots_total Total spots published to the broadcast bus.\n");
        out.push_str("# TYPE manta_spots_total counter\n");
        out.push_str(&format!(
            "manta_spots_total {}\n",
            self.spots_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_spots_by_band_total Spots published to the broadcast bus, by amateur band and spot type (sums to manta_spots_total).\n",
        );
        out.push_str("# TYPE manta_spots_by_band_total counter\n");
        for ((band, kind), count) in self
            .spots_by_band
            .lock()
            .expect("spots_by_band lock poisoned")
            .iter()
        {
            out.push_str(&format!(
                "manta_spots_by_band_total{{band=\"{band}\",type=\"{kind}\"}} {count}\n"
            ));
        }

        out.push_str(
            "# HELP manta_spots_dropped_lagged_total Spots (with [rbn_compare], spot grades too) dropped because a slow client fell behind and was disconnected.\n",
        );
        out.push_str("# TYPE manta_spots_dropped_lagged_total counter\n");
        out.push_str(&format!(
            "manta_spots_dropped_lagged_total {}\n",
            self.spots_dropped_lagged_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_spots_suppressed_by_filter_total Spots suppressed by a client's own filter (e.g. set dx filter unique), not a lag disconnect.\n",
        );
        out.push_str("# TYPE manta_spots_suppressed_by_filter_total counter\n");
        out.push_str(&format!(
            "manta_spots_suppressed_by_filter_total {}\n",
            self.spots_suppressed_by_filter_total
                .load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_spots_dropped_write_failed_total Spots (with [rbn_compare], spot grades too) dropped because a client's socket write timed out or failed before a lag/close error was observed.\n",
        );
        out.push_str("# TYPE manta_spots_dropped_write_failed_total counter\n");
        out.push_str(&format!(
            "manta_spots_dropped_write_failed_total {}\n",
            self.spots_dropped_write_failed_total
                .load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_spots_dropped_shutdown_total Spots (with [rbn_compare], spot grades too) abandoned because the daemon shut down while a client was still in its pre-login/handshake phase, before any socket write timed out or failed.\n",
        );
        out.push_str("# TYPE manta_spots_dropped_shutdown_total counter\n");
        out.push_str(&format!(
            "manta_spots_dropped_shutdown_total {}\n",
            self.spots_dropped_shutdown_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_spots_replay_abandoned_total `sh/dx` history entries abandoned without being replayed, because the replay write failed or the daemon shut down mid-replay. Counted separately from live-delivery loss: these are re-sends of spots already counted in manta_spots_total.\n",
        );
        out.push_str("# TYPE manta_spots_replay_abandoned_total counter\n");
        out.push_str(&format!(
            "manta_spots_replay_abandoned_total {}\n",
            self.spots_replay_abandoned_total.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP manta_spots_history_truncated_total Spots dropped from the sh/dx history at its 10000-spot ceiling while still inside [server].history_minutes. Non-zero means sh/dx reaches back less than the configured window.\n");
        out.push_str("# TYPE manta_spots_history_truncated_total counter\n");
        out.push_str(&format!(
            "manta_spots_history_truncated_total {}\n",
            self.spots_history_truncated_total()
        ));

        out.push_str(
            "# HELP manta_spots_unresolved_geography_total Spots emitted with an UNKNOWN_DXCC/UNKNOWN_CONTINENT/UNKNOWN_CQ_ZONE sentinel on the dx or de side, because the callsign did not resolve against cty.dat, OR its entity carries no row in the vendored dxcc.tsv, OR it carries a /MM or /AM designator that places it outside any DXCC entity.\n",
        );
        out.push_str("# TYPE manta_spots_unresolved_geography_total counter\n");
        out.push_str(&format!(
            "manta_spots_unresolved_geography_total {}\n",
            self.spots_unresolved_geography_total
                .load(Ordering::Relaxed)
        ));

        out.push_str("# HELP manta_telnet_clients_connected Currently connected telnet clients.\n");
        out.push_str("# TYPE manta_telnet_clients_connected gauge\n");
        out.push_str(&format!(
            "manta_telnet_clients_connected {}\n",
            self.telnet_clients.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_json_clients_connected Currently connected JSON Lines clients.\n",
        );
        out.push_str("# TYPE manta_json_clients_connected gauge\n");
        out.push_str(&format!(
            "manta_json_clients_connected {}\n",
            self.json_clients.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP manta_ws_clients_connected Currently connected WebSocket clients.\n");
        out.push_str("# TYPE manta_ws_clients_connected gauge\n");
        out.push_str(&format!(
            "manta_ws_clients_connected {}\n",
            self.ws_clients.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP manta_active_tracks Currently active decoder tracks.\n");
        out.push_str("# TYPE manta_active_tracks gauge\n");
        out.push_str(&format!(
            "manta_active_tracks {}\n",
            self.active_tracks.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_static_crashes_total Static crashes detected (>= 3 dB lift on at least three quarters of the passband's channels for >= 8 ms), counted once per crash.\n",
        );
        out.push_str("# TYPE manta_static_crashes_total counter\n");
        out.push_str(&format!(
            "manta_static_crashes_total {}\n",
            self.static_crashes_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_rtty_signals RTTY (two-tone FSK) signals currently marked by the detector, by amateur band; no CW tracks start on them.\n",
        );
        out.push_str("# TYPE manta_rtty_signals gauge\n");
        for (band, count) in self
            .rtty_by_band
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            out.push_str(&format!("manta_rtty_signals{{band=\"{band}\"}} {count}\n"));
        }

        out.push_str(
            "# HELP manta_source_health Per-input-source health (1 = healthy, 0 = unhealthy).\n",
        );
        out.push_str("# TYPE manta_source_health gauge\n");
        // MAN-96: one guard for the gauge and both outage families, so the
        // three render the same source set; `source_outages` is taken after
        // `source_health`, matching `set_source_health_at`'s lock order.
        let source_health = self
            .source_health
            .read()
            .expect("source_health lock poisoned");
        for (source, entry) in source_health.iter() {
            out.push_str(&format!(
                "manta_source_health{{source=\"{source}\"}} {}\n",
                if entry.healthy { 1 } else { 0 }
            ));
        }
        {
            let outages = self
                .source_outages
                .lock()
                .expect("source_outages lock poisoned");
            out.push_str(
                "# HELP manta_source_outages_total Times each input source went from healthy to unhealthy (a lost connection or failed read).\n",
            );
            out.push_str("# TYPE manta_source_outages_total counter\n");
            for source in source_health.keys() {
                let n = outages.get(source).map_or(0, |o| o.outages_total);
                out.push_str(&format!(
                    "manta_source_outages_total{{source=\"{source}\"}} {n}\n"
                ));
            }
            out.push_str(
                "# HELP manta_source_down_seconds_total Seconds each input source spent in outages that have ended; an outage still in progress is added when it ends.\n",
            );
            out.push_str("# TYPE manta_source_down_seconds_total counter\n");
            for source in source_health.keys() {
                let secs = outages.get(source).map_or(0.0, |o| o.down_seconds_total);
                out.push_str(&format!(
                    "manta_source_down_seconds_total{{source=\"{source}\"}} {secs:.3}\n"
                ));
            }
        }
        drop(source_health);

        // MAN-56: input-layer packet loss/malformed counters. Absent, not
        // zero, for sources with no wire-packet loss model -- the read
        // guard is taken once for all three families below (one consistent
        // view; three separate acquisitions could straddle a writer).
        let input_health = self
            .input_health
            .read()
            .expect("input_health lock poisoned");

        out.push_str(
            "# HELP manta_input_dropped_packets_total Input packets estimated lost in transit before reaching the decoder, per source.\n",
        );
        out.push_str("# TYPE manta_input_dropped_packets_total counter\n");
        for (source, h) in input_health.iter() {
            out.push_str(&format!(
                "manta_input_dropped_packets_total{{source=\"{source}\"}} {}\n",
                h.dropped_packets
            ));
        }

        out.push_str(
            "# HELP manta_input_gaps_detected_total Distinct input-stream gap events detected, per source (one gap may account for several dropped packets).\n",
        );
        out.push_str("# TYPE manta_input_gaps_detected_total counter\n");
        for (source, h) in input_health.iter() {
            out.push_str(&format!(
                "manta_input_gaps_detected_total{{source=\"{source}\"}} {}\n",
                h.gaps_detected
            ));
        }

        out.push_str(
            "# HELP manta_input_malformed_packets_total Input datagrams discarded as malformed -- wrong length, or correct length but failed framing/sync validation (MAN-22), per source.\n",
        );
        out.push_str("# TYPE manta_input_malformed_packets_total counter\n");
        for (source, h) in input_health.iter() {
            out.push_str(&format!(
                "manta_input_malformed_packets_total{{source=\"{source}\"}} {}\n",
                h.malformed_packets
            ));
        }

        drop(input_health);

        // MAN-316: the reference lists in use and their refresh history.
        // HELP/TYPE always render; each family has samples only for the
        // lists whose setter has run (see `ListMetrics`).
        let lists = self.list_metrics();
        out.push_str(
            "# HELP manta_list_info The reference list in use: list (cty or scp), the SHA-256 of its bytes, and its origin (bundled, file, cache or download). Always 1.\n",
        );
        out.push_str("# TYPE manta_list_info gauge\n");
        for (list, m) in lists.iter() {
            if let Some((sha256, origin)) = &m.in_use {
                out.push_str(&format!(
                    "manta_list_info{{list=\"{list}\",sha256=\"{}\",origin=\"{}\"}} 1\n",
                    escape_label_value(sha256),
                    origin.label()
                ));
            }
        }

        out.push_str(
            "# HELP manta_list_refresh_total Reference-list download attempts by outcome: updated, unchanged or failed.\n",
        );
        out.push_str("# TYPE manta_list_refresh_total counter\n");
        for (list, m) in lists.iter() {
            if let Some(totals) = &m.refresh_totals {
                for outcome in ListRefreshOutcome::ALL {
                    out.push_str(&format!(
                        "manta_list_refresh_total{{list=\"{list}\",outcome=\"{}\"}} {}\n",
                        outcome.label(),
                        totals[outcome.index()]
                    ));
                }
            }
        }

        out.push_str(
            "# HELP manta_list_last_success_timestamp_seconds Unix time the reference list in use was last confirmed current with its download URL.\n",
        );
        out.push_str("# TYPE manta_list_last_success_timestamp_seconds gauge\n");
        for (list, m) in lists.iter() {
            if let Some(at) = m.last_success {
                let secs = at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                out.push_str(&format!(
                    "manta_list_last_success_timestamp_seconds{{list=\"{list}\"}} {secs}\n"
                ));
            }
        }
        drop(lists);

        out.push_str("# HELP manta_uplink_sent_total Spots forwarded to the RBN uplink target.\n");
        out.push_str("# TYPE manta_uplink_sent_total counter\n");
        out.push_str(&format!(
            "manta_uplink_sent_total {}\n",
            self.uplink_sent_total()
        ));

        out.push_str(
            "# HELP manta_uplink_suppressed_total Spots not sent to the RBN uplink target because of dry-run or its spot_types setting.\n",
        );
        out.push_str("# TYPE manta_uplink_suppressed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_suppressed_total {}\n",
            self.uplink_suppressed_total()
        ));

        out.push_str(
            "# HELP manta_uplink_ceiling_suppressed_total Spots not sent to the RBN uplink target because its per-minute forwarding ceiling (max_spots_per_minute) was reached.\n",
        );
        out.push_str("# TYPE manta_uplink_ceiling_suppressed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_ceiling_suppressed_total {}\n",
            self.uplink_ceiling_suppressed_total()
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_lagged_total Spots the uplink fell behind on and lost before its next reconnect.\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_lagged_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_lagged_total {}\n",
            self.uplink_lagged_total()
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_write_failed_total Spots dropped because a write to the RBN uplink target's socket timed out or failed.\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_write_failed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_write_failed_total {}\n",
            self.uplink_write_failed_total()
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_disconnected_total Spots dropped when the RBN uplink connection was torn down for a reason other than a failed write (rate limit, protocol violation, stalled login, shutdown).\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_disconnected_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_disconnected_total {}\n",
            self.uplink_disconnected_total()
        ));

        out.push_str("# HELP manta_uplink_reconnects_total Times the RBN uplink connection was reestablished after dropping.\n");
        out.push_str("# TYPE manta_uplink_reconnects_total counter\n");
        out.push_str(&format!(
            "manta_uplink_reconnects_total {}\n",
            self.uplink_reconnects_total()
        ));

        out.push_str(
            "# HELP manta_uplink_connected Count of configured RBN uplink targets currently connected.\n",
        );
        out.push_str("# TYPE manta_uplink_connected gauge\n");
        out.push_str(&format!(
            "manta_uplink_connected {}\n",
            self.uplink_connected_count()
        ));

        // MAN-128: per-target uplink families -- one read guard for all
        // nine, so a concurrent `register_uplink_target` can't interleave
        // a partial view across them.
        let targets = self.uplink_targets_snapshot();

        out.push_str("# HELP manta_uplink_target_enabled Whether this configured RBN uplink target is enabled (1) or configured but disabled (0).\n");
        out.push_str("# TYPE manta_uplink_target_enabled gauge\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_enabled{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                if t.enabled() { 1 } else { 0 }
            ));
        }

        out.push_str("# HELP manta_uplink_target_connected Whether this RBN uplink target is currently connected.\n");
        out.push_str("# TYPE manta_uplink_target_connected gauge\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_connected{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                if t.connected() { 1 } else { 0 }
            ));
        }

        out.push_str(
            "# HELP manta_uplink_target_sent_total Spots forwarded to this RBN uplink target.\n",
        );
        out.push_str("# TYPE manta_uplink_target_sent_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_sent_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.sent_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_suppressed_total Spots not sent to this RBN uplink target because of dry-run or its spot_types setting.\n");
        out.push_str("# TYPE manta_uplink_target_suppressed_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_suppressed_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.suppressed_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_ceiling_suppressed_total Spots not sent to this RBN uplink target because its per-minute forwarding ceiling (max_spots_per_minute) was reached.\n");
        out.push_str("# TYPE manta_uplink_target_ceiling_suppressed_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_ceiling_suppressed_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.ceiling_suppressed_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_dropped_lagged_total Spots this target fell behind on and lost before its next reconnect.\n");
        out.push_str("# TYPE manta_uplink_target_dropped_lagged_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_dropped_lagged_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.lagged_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_dropped_write_failed_total Spots dropped because a write to this target's socket timed out or failed.\n");
        out.push_str("# TYPE manta_uplink_target_dropped_write_failed_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_dropped_write_failed_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.write_failed_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_dropped_disconnected_total Spots dropped when this target's connection was torn down for a reason other than a failed write.\n");
        out.push_str("# TYPE manta_uplink_target_dropped_disconnected_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_dropped_disconnected_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.disconnected_total()
            ));
        }

        out.push_str("# HELP manta_uplink_target_reconnects_total Times this RBN uplink target's connection was reestablished after dropping.\n");
        out.push_str("# TYPE manta_uplink_target_reconnects_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_reconnects_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.reconnects_total()
            ));
        }

        // MAN-44: the flapping-window gauge `GET /status` classifies on.
        let now = Instant::now();
        out.push_str(&format!(
            "# HELP manta_uplink_target_recent_reconnects Reconnects for this RBN uplink target within the last {}s (MAN-44 flapping window).\n",
            RECONNECT_WINDOW.as_secs()
        ));
        out.push_str("# TYPE manta_uplink_target_recent_reconnects gauge\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_recent_reconnects{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.recent_reconnects_at(now)
            ));
        }
        drop(targets);

        // MAN-128: process uptime/build-info.
        out.push_str(
            "# HELP manta_start_time_seconds Unix time at which this daemon's spot server and metrics endpoint started.\n",
        );
        out.push_str("# TYPE manta_start_time_seconds gauge\n");
        out.push_str(&format!(
            "manta_start_time_seconds {:.3}\n",
            self.started.unix_seconds
        ));

        out.push_str(
            "# HELP manta_uptime_seconds Seconds since this daemon's spot server and metrics endpoint started (monotonic).\n",
        );
        out.push_str("# TYPE manta_uptime_seconds gauge\n");
        out.push_str(&format!(
            "manta_uptime_seconds {:.3}\n",
            self.started.instant.elapsed().as_secs_f64()
        ));

        out.push_str(
            "# HELP manta_build_info Build metadata (value is always 1): crate version, git commit, and compiled-in optional features.\n",
        );
        out.push_str("# TYPE manta_build_info gauge\n");
        if let Some(info) = self
            .build_info
            .read()
            .expect("build_info lock poisoned")
            .as_ref()
        {
            out.push_str(&format!(
                "manta_build_info{{version=\"{}\",git_sha=\"{}\",features=\"{}\"}} 1\n",
                escape_label_value(&info.version),
                escape_label_value(&info.git_sha),
                escape_label_value(&info.features)
            ));
        }

        // MAN-128: decode-latency histogram.
        out.push_str(
            "# HELP manta_decode_latency_seconds Wall-clock time the decode thread spent processing one input chunk (channelize, track, decode, validate), excluding the wait for source data. Keeping up with real time requires this to stay below the chunk duration (2048 samples / sample rate).\n",
        );
        out.push_str("# TYPE manta_decode_latency_seconds histogram\n");
        if let Some(h) = self
            .decode_latency
            .read()
            .expect("decode_latency lock poisoned")
            .as_ref()
        {
            let mut cumulative = 0u64;
            for (bound, count) in h.bounds_seconds.iter().zip(h.bucket_counts.iter()) {
                cumulative += count;
                out.push_str(&format!(
                    "manta_decode_latency_seconds_bucket{{le=\"{bound}\"}} {cumulative}\n"
                ));
            }
            cumulative += h.bucket_counts.last().copied().unwrap_or(0);
            out.push_str(&format!(
                "manta_decode_latency_seconds_bucket{{le=\"+Inf\"}} {cumulative}\n"
            ));
            out.push_str(&format!(
                "manta_decode_latency_seconds_sum {}\n",
                h.sum_seconds
            ));
            out.push_str(&format!(
                "manta_decode_latency_seconds_count {cumulative}\n"
            ));
        }

        // MAN-118: decode-quality families. Literal `# TYPE` lines, one per
        // family, so `docs_consistency.rs` can find each one in this source.
        let quality = self
            .decode_quality
            .read()
            .expect("decode_quality lock poisoned")
            .clone();
        out.push_str("# HELP manta_decode_chars_total Characters decoded across every track.\n");
        out.push_str("# TYPE manta_decode_chars_total counter\n");
        if let Some(q) = &quality {
            out.push_str(&format!("manta_decode_chars_total {}\n", q.chars_total));
        }
        out.push_str(
            "# HELP manta_decode_garbles_total Characters the decoder could not match to any Morse character (every beam branch rejected the marks), across every track.\n",
        );
        out.push_str("# TYPE manta_decode_garbles_total counter\n");
        if let Some(q) = &quality {
            out.push_str(&format!("manta_decode_garbles_total {}\n", q.garbles_total));
        }
        out.push_str(
            "# HELP manta_track_rail_ratio Keying-rail ratio (mark rail / space rail amplitude) of each decoding track, sampled once per second per track. 2 is the floor the rails collapse to.\n",
        );
        out.push_str("# TYPE manta_track_rail_ratio histogram\n");
        if let Some(q) = &quality {
            push_histogram_samples(&mut out, "manta_track_rail_ratio", &q.rail_ratio);
        }
        out.push_str(
            "# HELP manta_candidate_repetitions Message-distinct decodes of the same call near the same frequency within the trailing 90 s, for each callsign candidate the repetition gate scored. A candidate needs 2, 3 or 5 to spot by its callsign-pattern tier (2 for every call at validation_level minimal).\n",
        );
        out.push_str("# TYPE manta_candidate_repetitions histogram\n");
        if let Some(q) = &quality {
            push_histogram_samples(&mut out, "manta_candidate_repetitions", &q.repetitions);
        }

        // MAN-128: health gauges, computed from the SAME `health_at` that
        // backs `/healthz` -- a Prometheus-only operator never sees a
        // different verdict than the probe. Computed before any lock in
        // this function is still held (health_at takes its own).
        let report = self.health_at(Instant::now());
        out.push_str(
            "# HELP manta_healthy Whether /healthz currently reports this node healthy.\n",
        );
        out.push_str("# TYPE manta_healthy gauge\n");
        out.push_str(&format!(
            "manta_healthy {}\n",
            if report.healthy { 1 } else { 0 }
        ));

        out.push_str(
            "# HELP manta_listener_up Whether this listener (telnet/json/metrics) is currently up.\n",
        );
        out.push_str("# TYPE manta_listener_up gauge\n");
        for (name, up) in self
            .listener_up
            .read()
            .expect("listener_up lock poisoned")
            .iter()
        {
            out.push_str(&format!(
                "manta_listener_up{{listener=\"{name}\"}} {}\n",
                if *up { 1 } else { 0 }
            ));
        }

        self.render_rbn_compare_at(&mut out, Instant::now());
        self.render_live(&mut out);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn spot(freq_hz: f64, spot_type: SpotType) -> manta_spot::Spot {
        manta_spot::Spot {
            callsign: "W1AW".to_string(),
            freq_hz,
            snr_db: 20.0,
            wpm: 20.0,
            spot_type,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    /// MAN-45 (PR #63 round-16 finding, revised round-18 remediate finding
    /// 1): the shapes every write-failure/clean-shutdown site in
    /// `telnet`/`json_stream` needs, in one place: a failed LIVE-SPOT write
    /// loses the in-flight spot too; a failed CONTROL-frame write (telnet's
    /// filter ack, the WS Pong reply) or a clean-shutdown disconnect loses
    /// only the queue; and `sh/dx`'s history-replay write failure is the
    /// SAME shape as a control-frame write, not a spot write -- the entry
    /// being written is a replay of an already-published, already-counted
    /// spot, so neither it nor the rest of the unreplayed history iterator
    /// is newly-lost data.
    #[test]
    fn abandoned_spot_count_covers_every_write_failure_and_shutdown_shape() {
        // Live-spot write: the in-flight spot plus everything still retained.
        assert_eq!(abandoned_spot_count(true, 7), 8);
        // Control-frame write, or a clean shutdown with no write attempted:
        // nothing was in flight, the queue is still lost.
        assert_eq!(abandoned_spot_count(false, 7), 7);
        // Nothing queued, control frame: nothing to charge.
        assert_eq!(abandoned_spot_count(false, 0), 0);
        // `sh/dx` history-replay write failure: same shape as a
        // control-frame write -- the failed write and the rest of the
        // history iterator are replays, not newly-lost live spots, so only
        // the live `rx` backlog counts.
        assert_eq!(abandoned_spot_count(false, 7), 7);
    }

    #[test]
    fn renders_spot_count_as_a_prometheus_counter() {
        let m = Metrics::new();
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_total counter"));
        assert!(text.contains("manta_spots_total 2"));
    }

    /// MAN-128 Scenario 1: spots broken out by band and type, additive to
    /// (not a replacement for) `manta_spots_total`.
    #[test]
    fn record_spot_breaks_counts_out_by_band_and_type() {
        let m = Metrics::new();
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.record_spot(&spot(7_030_000.0, SpotType::De));
        m.record_spot(&spot(1_000.0, SpotType::Unknown));
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_by_band_total counter"));
        assert!(text.contains(r#"manta_spots_by_band_total{band="20m",type="cq"} 2"#));
        assert!(text.contains(r#"manta_spots_by_band_total{band="40m",type="de"} 1"#));
        assert!(text.contains(r#"manta_spots_by_band_total{band="unknown",type="unknown"} 1"#));
        assert!(text.contains("manta_spots_total 4"));
    }

    #[test]
    fn spots_by_band_sum_equals_spots_total() {
        let m = Metrics::new();
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.record_spot(&spot(7_030_000.0, SpotType::De));
        m.record_spot(&spot(21_025_000.0, SpotType::Beacon));
        let text = m.render_prometheus_text();
        let by_band_sum: u64 = text
            .lines()
            .filter(|l| l.starts_with("manta_spots_by_band_total{"))
            .map(|l| l.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
            .sum();
        let total: u64 = text
            .lines()
            .find(|l| l.starts_with("manta_spots_total "))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(by_band_sum, total);
    }

    #[test]
    fn band_label_uses_the_same_rounding_as_the_json_stream() {
        let m = Metrics::new();
        m.record_spot(&spot(13_999_999.6, SpotType::Cq)); // rounds to 14_000_000.0
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_spots_by_band_total{band="20m",type="cq"} 1"#));
    }

    #[test]
    fn rtty_signals_render_per_band() {
        let m = Metrics::new();
        m.set_rtty_signals(&[14_080_002.4, 14_086_002.3, 7_045_000.0]);
        let text = m.render_prometheus_text();
        assert!(text.contains("# HELP manta_rtty_signals RTTY (two-tone FSK) signals"));
        assert!(text.contains("# TYPE manta_rtty_signals gauge"));
        assert!(text.contains(r#"manta_rtty_signals{band="20m"} 2"#));
        assert!(text.contains(r#"manta_rtty_signals{band="40m"} 1"#));
    }

    #[test]
    fn rtty_band_series_stays_at_zero_after_clear() {
        let m = Metrics::new();
        m.set_rtty_signals(&[14_080_002.4]);
        m.set_rtty_signals(&[]);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_rtty_signals{band="20m"} 0"#));
    }

    #[test]
    fn no_rtty_samples_before_first_mark() {
        let text = Metrics::new().render_prometheus_text();
        assert!(text.contains("# TYPE manta_rtty_signals gauge"));
        assert!(!text.contains("manta_rtty_signals{"));
    }

    #[test]
    fn spots_by_band_family_has_header_but_no_samples_before_any_spot() {
        let text = Metrics::new().render_prometheus_text();
        assert!(text.contains("# HELP manta_spots_by_band_total"));
        assert!(text.contains("# TYPE manta_spots_by_band_total counter"));
        assert!(!text.contains("manta_spots_by_band_total{"));
    }

    #[test]
    fn beacon_type_maps_to_lowercase_label() {
        let m = Metrics::new();
        m.record_spot(&spot(14_025_000.0, SpotType::Beacon));
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_spots_by_band_total{band="20m",type="beacon"} 1"#));
    }

    #[test]
    fn renders_lagged_drop_count_as_a_prometheus_counter() {
        let m = Metrics::new();
        m.record_lagged(3);
        m.record_lagged(4);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_dropped_lagged_total counter"));
        assert!(text.contains("manta_spots_dropped_lagged_total 7"));
    }

    #[test]
    fn live_families_render_only_once_enabled_and_never_touch_spot_loss() {
        let m = Metrics::new();
        m.record_live_lagged(5);
        m.inc_live_clients();
        assert!(!m.render_prometheus_text().contains("manta_live_"));
        m.enable_live();
        m.inc_live_clients();
        m.record_live_published("trackState");
        m.record_live_published("trackState");
        m.record_live_lagged(7);
        m.record_live_write_failed(2);
        m.record_live_dropped_shutdown(1);
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_live_clients_connected 1\n"), "{text}");
        assert!(text.contains("manta_live_records_total{kind=\"trackState\"} 2\n"));
        assert!(text.contains("manta_live_records_total{kind=\"bandState\"} 0\n"));
        assert!(text.contains("manta_live_records_dropped_total{reason=\"lagged\"} 7\n"));
        assert!(text.contains("manta_live_records_dropped_total{reason=\"write_failed\"} 2\n"));
        assert!(text.contains("manta_live_records_dropped_total{reason=\"shutdown\"} 1\n"));
        assert!(text.contains("manta_spots_dropped_lagged_total 0\n"));
        assert_eq!(m.live_records_dropped(), 10);
    }

    #[test]
    fn renders_connected_client_gauges_per_protocol() {
        let m = Metrics::new();
        m.inc_telnet_clients();
        m.inc_telnet_clients();
        m.dec_telnet_clients();
        m.inc_json_clients();
        m.inc_ws_clients();
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_telnet_clients_connected 1"));
        assert!(text.contains("manta_json_clients_connected 1"));
        assert!(text.contains("manta_ws_clients_connected 1"));
    }

    #[test]
    fn renders_injected_active_track_count() {
        let m = Metrics::new();
        m.set_active_tracks(12);
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_active_tracks 12"));
    }

    #[test]
    fn static_crashes_total_never_goes_backwards() {
        let m = Metrics::new();
        assert!(m
            .render_prometheus_text()
            .contains("\nmanta_static_crashes_total 0\n"));
        m.set_static_crashes_total(3);
        m.set_static_crashes_total(2);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_static_crashes_total counter\n"));
        assert!(text.contains("\nmanta_static_crashes_total 3\n"), "{text}");
    }

    #[test]
    fn renders_filter_suppressed_count_as_a_prometheus_counter() {
        let m = Metrics::new();
        m.record_filter_suppressed(1);
        m.record_filter_suppressed(1);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_suppressed_by_filter_total counter"));
        assert!(text.contains("manta_spots_suppressed_by_filter_total 2"));
    }

    #[test]
    fn renders_write_failed_count_as_a_prometheus_counter() {
        let m = Metrics::new();
        m.record_write_failed(3);
        m.record_write_failed(4);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_dropped_write_failed_total counter"));
        assert!(text.contains("manta_spots_dropped_write_failed_total 7"));
    }

    /// MAN-45 remediate (code-review round 18, finding 2): a clean-shutdown
    /// disconnect must count separately from a genuine write failure, so an
    /// operator reading `spots_dropped_write_failed_total`'s "socket write
    /// timed out or failed" HELP text never sees a shutdown-only session
    /// misattributed to it.
    #[test]
    fn renders_dropped_shutdown_count_as_a_prometheus_counter_distinct_from_write_failed() {
        let m = Metrics::new();
        m.record_dropped_shutdown(2);
        m.record_dropped_shutdown(5);
        assert_eq!(m.spots_dropped_shutdown_total(), 7);
        assert_eq!(m.spots_dropped_write_failed_total(), 0);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_dropped_shutdown_total counter"));
        assert!(text.contains("manta_spots_dropped_shutdown_total 7"));
    }

    /// MAN-45 (round-6 review finding).
    /// MAN-45 remediate (code-review round 19, P2): abandoned `sh/dx`
    /// replay entries must be VISIBLE (ARCHITECTURE §8) without inflating
    /// `manta_spots_dropped_write_failed_total`, whose "delivered +
    /// counted == published" invariant would break if re-sends of
    /// already-published spots were summed into it.
    #[test]
    fn replay_abandoned_is_counted_separately_from_write_failed() {
        let m = Metrics::new();
        m.record_replay_abandoned(1 + 4);
        m.record_replay_abandoned(3);
        assert_eq!(m.spots_replay_abandoned_total(), 8);
        assert_eq!(m.spots_dropped_write_failed_total(), 0);
        assert_eq!(m.spots_dropped_shutdown_total(), 0);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_replay_abandoned_total counter"));
        assert!(text.contains("manta_spots_replay_abandoned_total 8"));
        assert!(text.contains("manta_spots_dropped_write_failed_total 0"));
    }

    #[test]
    fn unresolved_geography_is_counted_and_exposed() {
        let m = Metrics::new();
        m.record_unresolved_geography();
        m.record_unresolved_geography();
        assert!(m
            .render_prometheus_text()
            .contains("manta_spots_unresolved_geography_total 2"));
    }

    #[test]
    fn unresolved_geography_counter_is_present_at_zero_before_any_spot() {
        // A counter that only appears once it fires is invisible to an operator
        // building a dashboard -- the other spot counters all render at 0.
        assert!(Metrics::new()
            .render_prometheus_text()
            .contains("manta_spots_unresolved_geography_total 0"));
    }

    #[test]
    fn renders_per_source_health_as_labeled_gauge() {
        let m = Metrics::new();
        m.set_source_health("soapy0", true);
        m.set_source_health("kiwi-remote", false);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_source_health{source="kiwi-remote"} 0"#));
        assert!(text.contains(r#"manta_source_health{source="soapy0"} 1"#));
    }

    // MAN-96: source-outage counters.

    #[test]
    fn source_outage_counters_count_healthy_to_unhealthy_edges_and_completed_down_time() {
        let m = Metrics::new();
        let t0 = Instant::now();
        m.set_source_health_at("soapy", true, t0);
        m.set_source_health_at("soapy", false, t0 + Duration::from_secs(10));
        m.set_source_health_at("soapy", true, t0 + Duration::from_millis(22_500));
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="soapy"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_source_down_seconds_total{source="soapy"} 12.500"#),
            "{text}"
        );
    }

    #[test]
    fn initial_unhealthy_report_is_not_an_outage() {
        // HPSDR starts unhealthy until confirmed live: absent -> false -> true
        // is start-up, not an outage.
        let m = Metrics::new();
        let t0 = Instant::now();
        m.set_source_health_at("hpsdr", false, t0);
        m.set_source_health_at("hpsdr", true, t0 + Duration::from_secs(3));
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="hpsdr"} 0"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_source_down_seconds_total{source="hpsdr"} 0.000"#),
            "{text}"
        );
    }

    #[test]
    fn repeated_unhealthy_reports_do_not_double_count() {
        // true, false, false, true -> 1 outage, down time measured from the
        // FIRST false.
        let m = Metrics::new();
        let t0 = Instant::now();
        m.set_source_health_at("kiwi", true, t0);
        m.set_source_health_at("kiwi", false, t0 + Duration::from_secs(1));
        m.set_source_health_at("kiwi", false, t0 + Duration::from_secs(4));
        m.set_source_health_at("kiwi", true, t0 + Duration::from_secs(6));
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="kiwi"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_source_down_seconds_total{source="kiwi"} 5.000"#),
            "{text}"
        );
    }

    #[test]
    fn outage_counters_render_zero_for_a_never_down_source_with_type_lines() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        let text = m.render_prometheus_text();
        assert!(
            text.contains("# TYPE manta_source_outages_total counter\n"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE manta_source_down_seconds_total counter\n"),
            "{text}"
        );
        assert!(
            text.contains("manta_source_outages_total{source=\"file\"} 0\n"),
            "{text}"
        );
        assert!(
            text.contains("manta_source_down_seconds_total{source=\"file\"} 0.000\n"),
            "{text}"
        );
    }

    #[test]
    fn an_outage_in_progress_is_not_counted_in_down_seconds_until_it_ends() {
        let m = Metrics::new();
        let t0 = Instant::now();
        m.set_source_health_at("soapy", true, t0);
        m.set_source_health_at("soapy", false, t0 + Duration::from_secs(2));
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="soapy"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_source_down_seconds_total{source="soapy"} 0.000"#),
            "{text}"
        );
    }

    /// MAN-64 round 7 (V-2): a regular `set_source_health(true)` landing
    /// after the fatal-exit `set_source_health_terminal(false)` must never
    /// win, regardless of which one happens to run last.
    #[test]
    fn terminal_source_health_cannot_be_overwritten_by_a_later_regular_write() {
        let m = Metrics::new();
        m.set_source_health("hpsdr", true);
        m.set_source_health_terminal("hpsdr", false);
        // A regular (non-terminal) write arriving after the terminal write
        // has already landed.
        m.set_source_health("hpsdr", true);
        assert!(m
            .render_prometheus_text()
            .contains(r#"manta_source_health{source="hpsdr"} 0"#));
    }

    /// MAN-96 x MAN-64: a fatal-exit terminal write is a real outage edge,
    /// and a regular write after it moves neither the gauge nor the
    /// outage counters.
    #[test]
    fn terminal_unhealthy_write_counts_one_outage_and_freezes_the_counters() {
        let m = Metrics::new();
        m.set_source_health("hpsdr", true);
        m.set_source_health_terminal("hpsdr", false);
        m.set_source_health("hpsdr", true);
        m.set_source_health("hpsdr", false);
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_source_outages_total{source="hpsdr"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_source_down_seconds_total{source="hpsdr"} 0.000"#),
            "{text}"
        );
        assert!(text.contains(r#"manta_source_health{source="hpsdr"} 0"#));
    }

    // MAN-56: input-layer packet loss/malformed counters.

    #[test]
    fn renders_per_source_input_health_as_labeled_counters() {
        let m = Metrics::new();
        m.set_input_health(
            "hpsdr",
            InputHealth {
                dropped_packets: 12,
                gaps_detected: 3,
                malformed_packets: 4,
            },
        );
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_input_dropped_packets_total counter"));
        assert!(text.contains(r#"manta_input_dropped_packets_total{source="hpsdr"} 12"#));
        assert!(text.contains(r#"manta_input_gaps_detected_total{source="hpsdr"} 3"#));
        assert!(text.contains(r#"manta_input_malformed_packets_total{source="hpsdr"} 4"#));
    }

    #[test]
    fn input_health_reflects_the_latest_absolute_totals_not_a_sum() {
        // The wiring layer pushes the source's current monotonic totals on
        // a timer; two pushes must not double-count (MAN-56 D4).
        let m = Metrics::new();
        m.set_input_health(
            "hpsdr",
            InputHealth {
                dropped_packets: 2,
                ..Default::default()
            },
        );
        m.set_input_health(
            "hpsdr",
            InputHealth {
                dropped_packets: 5,
                ..Default::default()
            },
        );
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_input_dropped_packets_total{source="hpsdr"} 5"#));
        assert!(!text.contains(r#"manta_input_dropped_packets_total{source="hpsdr"} 7"#));
    }

    #[test]
    fn sources_that_never_report_input_health_publish_no_series() {
        // An absent series, not a frozen 0 -- ARCHITECTURE §8 already flags
        // "served but never populated" (manta_active_tracks) as actively
        // misleading to an operator.
        let m = Metrics::new();
        m.set_source_health("audio", true);
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_input_malformed_packets_total counter"));
        assert!(!text.contains("manta_input_malformed_packets_total{"));
    }

    #[test]
    fn input_health_families_render_their_samples_contiguously() {
        // Prometheus text format requires all samples of one metric family
        // to be grouped; three sources interleaved per-family would be
        // malformed.
        let m = Metrics::new();
        m.set_input_health(
            "hpsdr",
            InputHealth {
                dropped_packets: 1,
                gaps_detected: 1,
                malformed_packets: 1,
            },
        );
        let text = m.render_prometheus_text();
        let dropped = text.find("manta_input_dropped_packets_total{").unwrap();
        let gaps_help = text.find("# HELP manta_input_gaps_detected_total").unwrap();
        assert!(dropped < gaps_help);
    }

    // MAN-32/MAN-128: RBN uplink counters, now registry-backed.

    #[test]
    fn uplink_aggregates_are_sums_over_registered_targets() {
        let m = Metrics::new();
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
        let b = m.register_uplink_target("b.example:7000".to_string(), true);

        a.record_sent();
        a.record_sent();
        b.record_sent();
        b.record_reconnect();
        b.record_reconnect();
        b.record_reconnect();
        a.mark_connected();

        assert_eq!(m.uplink_sent_total(), 3);
        assert_eq!(m.uplink_reconnects_total(), 3);
        assert!(m.uplink_connected());
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_uplink_sent_total 3"));
        assert!(text.contains("manta_uplink_reconnects_total 3"));
        assert!(text.contains("manta_uplink_connected 1"));
    }

    /// MAN-248: ceiling suppression is its own counter -- rendered as an
    /// aggregate family and a per-target family -- and never leaks into
    /// the combined `suppressed` counters, whose HELP text promises
    /// "dry-run or spot_types" only.
    #[test]
    fn ceiling_suppression_renders_and_stays_out_of_the_combined_suppressed_counter() {
        let m = Metrics::new();
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
        let b = m.register_uplink_target("b.example:7000".to_string(), true);

        a.record_ceiling_suppressed();
        a.record_ceiling_suppressed();
        a.record_ceiling_suppressed();
        b.record_ceiling_suppressed();
        b.record_ceiling_suppressed();
        // Ordinary suppression (dry-run / spot_types) on `a` must not move
        // its ceiling counter, and vice versa.
        a.record_suppressed();

        assert_eq!(m.uplink_ceiling_suppressed_total(), 5);
        assert_eq!(a.ceiling_suppressed_total(), 3);
        assert_eq!(b.ceiling_suppressed_total(), 2);
        assert_eq!(
            a.suppressed_total(),
            1,
            "ordinary suppression must not move the ceiling counter"
        );

        let text = m.render_prometheus_text();
        assert!(text.contains("manta_uplink_ceiling_suppressed_total 5\n"));
        assert!(text.contains(
            r#"manta_uplink_target_ceiling_suppressed_total{target="a.example:7000"} 3"#
        ));
        assert!(text.contains(
            r#"manta_uplink_target_ceiling_suppressed_total{target="b.example:7000"} 2"#
        ));
        // The combined counters are unchanged by ceiling records: the
        // aggregate is 1 (only `a`'s ordinary record), never 6.
        assert!(text.contains("manta_uplink_suppressed_total 1\n"));
        assert!(text.contains(r#"manta_uplink_target_suppressed_total{target="a.example:7000"} 1"#));

        // A target that only ever records ordinary suppression renders 0
        // in the ceiling families rather than inheriting `suppressed`.
        let only_ordinary = Metrics::new();
        let t = only_ordinary.register_uplink_target("c.example:7000".to_string(), true);
        t.record_suppressed();
        t.record_suppressed();
        let text = only_ordinary.render_prometheus_text();
        assert!(text.contains("manta_uplink_ceiling_suppressed_total 0\n"));
        assert!(text.contains(
            r#"manta_uplink_target_ceiling_suppressed_total{target="c.example:7000"} 0"#
        ));
        assert_eq!(t.ceiling_suppressed_total(), 0);
    }

    #[test]
    fn renders_per_target_uplink_families_with_labels() {
        let m = Metrics::new();
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
        let b = m.register_uplink_target("b.example:7000".to_string(), true);
        a.mark_connected();
        b.record_reconnect();
        b.record_reconnect();
        b.record_reconnect();

        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_uplink_target_connected{target="a.example:7000"} 1"#));
        assert!(text.contains(r#"manta_uplink_target_connected{target="b.example:7000"} 0"#));
        assert!(text.contains(r#"manta_uplink_target_reconnects_total{target="b.example:7000"} 3"#));
    }

    #[test]
    fn disabled_target_is_rendered_with_enabled_zero() {
        let m = Metrics::new();
        m.register_uplink_target("a.example:7000".to_string(), false);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_uplink_target_enabled{target="a.example:7000"} 0"#));
    }

    #[test]
    fn uplink_target_label_is_escaped() {
        let m = Metrics::new();
        m.register_uplink_target(r#"a"host:7000"#.to_string(), true);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"target="a\"host:7000""#));
    }

    #[test]
    fn per_target_families_render_contiguously_in_registration_order() {
        let m = Metrics::new();
        m.register_uplink_target("a:1".to_string(), true);
        m.register_uplink_target("b:2".to_string(), true);
        let text = m.render_prometheus_text();
        let enabled_header = text.find("# TYPE manta_uplink_target_enabled").unwrap();
        let connected_header = text.find("# TYPE manta_uplink_target_connected").unwrap();
        let a_enabled = text
            .find(r#"manta_uplink_target_enabled{target="a:1"}"#)
            .unwrap();
        let b_enabled = text
            .find(r#"manta_uplink_target_enabled{target="b:2"}"#)
            .unwrap();
        assert!(enabled_header < a_enabled);
        assert!(a_enabled < b_enabled);
        assert!(b_enabled < connected_header);
    }

    // MAN-42 (structural now, MAN-128): each target owns its own
    // `connected` flag, so one target's failed reconnects can never touch
    // another's -- the pre-MAN-128 shared-atomic desync class is no longer
    // reachable, not just avoided by calling-convention discipline.
    #[test]
    fn uplink_connected_reflects_multiple_independently_tracked_targets() {
        let m = Metrics::new();
        assert!(!m.uplink_connected());

        let a = m.register_uplink_target("a".to_string(), true);
        let b = m.register_uplink_target("b".to_string(), true);

        a.mark_connected();
        assert!(m.uplink_connected());

        b.record_reconnect();
        b.record_reconnect();
        b.record_reconnect();
        assert!(
            m.uplink_connected(),
            "an unrelated target's failed reconnects must not clear the gauge \
             while another target is genuinely connected"
        );

        a.mark_disconnected();
        assert!(!m.uplink_connected());
    }

    // MAN-122: the periodic status line reads these getters directly
    // instead of parsing render_prometheus_text() back out of a String.
    #[test]
    fn live_state_getters_read_back_what_the_recorders_wrote() {
        let m = Metrics::new();
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.record_spot(&spot(14_025_000.0, SpotType::Cq));
        m.set_active_tracks(4);
        m.inc_telnet_clients();
        m.inc_json_clients();
        m.inc_ws_clients();
        m.dec_ws_clients();
        assert_eq!(m.spots_total(), 2);
        assert_eq!(m.active_tracks(), 4);
        assert_eq!(
            (m.telnet_clients(), m.json_clients(), m.ws_clients()),
            (1, 1, 0)
        );
    }

    // MAN-122 review round 2: the status line's stall detection compares
    // this counter against its own previous sample, so the only property
    // that matters is that it advances once per recorded batch and never
    // goes backwards.
    #[test]
    fn the_pipeline_batch_counter_advances_once_per_batch() {
        let m = Metrics::new();
        assert_eq!(m.pipeline_batches(), 0);
        m.record_pipeline_batch();
        m.record_pipeline_batch();
        assert_eq!(m.pipeline_batches(), 2);
    }

    #[test]
    fn no_targets_registered_renders_aggregates_at_zero_and_per_target_headers_only() {
        let text = Metrics::new().render_prometheus_text();
        assert!(text.contains("manta_uplink_sent_total 0"));
        assert!(text.contains("manta_uplink_connected 0"));
        assert!(text.contains("# TYPE manta_uplink_target_enabled gauge"));
        assert!(!text.contains("manta_uplink_target_enabled{"));
    }

    // MAN-128: uptime/build-info.

    #[test]
    fn renders_start_time_and_uptime_gauges() {
        let m = Metrics::new();
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_start_time_seconds gauge"));
        assert!(text.contains("# TYPE manta_uptime_seconds gauge"));

        let start: f64 = text
            .lines()
            .find(|l| l.starts_with("manta_start_time_seconds "))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!((start - now).abs() < 5.0);

        let uptime: f64 = text
            .lines()
            .find(|l| l.starts_with("manta_uptime_seconds "))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(uptime >= 0.0);
    }

    #[test]
    fn uptime_increases_monotonically() {
        let m = Metrics::new();
        let first = m.render_prometheus_text();
        std::thread::sleep(Duration::from_millis(20));
        let second = m.render_prometheus_text();
        let get = |t: &str| -> f64 {
            t.lines()
                .find(|l| l.starts_with("manta_uptime_seconds "))
                .unwrap()
                .rsplit(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert!(get(&second) > get(&first));
    }

    #[test]
    fn build_info_is_absent_until_set_then_rendered_as_info_metric() {
        let m = Metrics::new();
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_build_info gauge"));
        assert!(!text.contains("manta_build_info{"));

        m.set_build_info(BuildInfo {
            version: "0.1.0".to_string(),
            git_sha: "abc123".to_string(),
            features: "hpsdr".to_string(),
        });
        let text = m.render_prometheus_text();
        assert!(text
            .contains(r#"manta_build_info{version="0.1.0",git_sha="abc123",features="hpsdr"} 1"#));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");

        let m = Metrics::new();
        m.set_build_info(BuildInfo {
            version: "0.1.0".to_string(),
            git_sha: "a\"b".to_string(),
            features: "none".to_string(),
        });
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"git_sha="a\"b""#));
    }

    // MAN-128: decode-latency histogram.

    #[test]
    fn decode_latency_renders_cumulative_prometheus_histogram() {
        let m = Metrics::new();
        m.set_decode_latency(LatencyHistogram {
            bounds_seconds: vec![0.001, 0.01],
            bucket_counts: vec![2, 3, 1],
            sum_seconds: 0.05,
        });
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_decode_latency_seconds histogram"));
        assert!(text.contains(r#"manta_decode_latency_seconds_bucket{le="0.001"} 2"#));
        assert!(text.contains(r#"manta_decode_latency_seconds_bucket{le="0.01"} 5"#));
        assert!(text.contains(r#"manta_decode_latency_seconds_bucket{le="+Inf"} 6"#));
        assert!(text.contains("manta_decode_latency_seconds_sum 0.05"));
        assert!(text.contains("manta_decode_latency_seconds_count 6"));
    }

    #[test]
    fn histogram_count_always_equals_inf_bucket() {
        let m = Metrics::new();
        m.set_decode_latency(LatencyHistogram {
            bounds_seconds: vec![0.001],
            bucket_counts: vec![4, 2],
            sum_seconds: 0.01,
        });
        let text = m.render_prometheus_text();
        let inf: u64 = text
            .lines()
            .find(|l| l.contains(r#"le="+Inf""#))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let count: u64 = text
            .lines()
            .find(|l| l.starts_with("manta_decode_latency_seconds_count "))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(inf, count);
    }

    // MAN-118: decode-quality families.

    fn sample_decode_quality() -> DecodeQuality {
        DecodeQuality {
            chars_total: 9690,
            garbles_total: 3,
            rail_ratio: ValueHistogram {
                bounds: vec![2.0, 2.5, 63.0],
                bucket_counts: vec![6, 304, 5725, 4],
                sum: 61022.4,
            },
            repetitions: ValueHistogram {
                bounds: vec![1.0, 2.0],
                bucket_counts: vec![116, 38, 120],
                sum: 899.0,
            },
        }
    }

    #[test]
    fn repetition_help_describes_pattern_tiers_and_minimal_level() {
        let text = Metrics::new().render_prometheus_text();
        assert!(text.contains("A candidate needs 2, 3 or 5 to spot by its callsign-pattern tier (2 for every call at validation_level minimal)."));
    }

    #[test]
    fn decode_quality_families_render_header_only_when_never_set() {
        let text = Metrics::new().render_prometheus_text();
        for (name, kind) in [
            ("manta_decode_chars_total", "counter"),
            ("manta_decode_garbles_total", "counter"),
            ("manta_track_rail_ratio", "histogram"),
            ("manta_candidate_repetitions", "histogram"),
        ] {
            assert!(text.contains(&format!("# HELP {name} ")), "{name} HELP");
            assert!(
                text.contains(&format!("# TYPE {name} {kind}\n")),
                "{name} TYPE"
            );
            assert!(
                !text.lines().any(|l| l.starts_with(name)),
                "{name} must render no samples before set_decode_quality"
            );
        }
    }

    #[test]
    fn decode_quality_counters_render_their_values() {
        let m = Metrics::new();
        m.set_decode_quality(sample_decode_quality());
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_decode_chars_total 9690\n"));
        assert!(text.contains("manta_decode_garbles_total 3\n"));
    }

    #[test]
    fn decode_quality_histograms_are_cumulative_with_inf_equal_to_count() {
        let m = Metrics::new();
        m.set_decode_quality(sample_decode_quality());
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_track_rail_ratio_bucket{le="2"} 6"#));
        assert!(text.contains(r#"manta_track_rail_ratio_bucket{le="2.5"} 310"#));
        assert!(text.contains(r#"manta_track_rail_ratio_bucket{le="63"} 6035"#));
        assert!(text.contains(r#"manta_track_rail_ratio_bucket{le="+Inf"} 6039"#));
        assert!(text.contains("manta_track_rail_ratio_sum 61022.4\n"));
        assert!(text.contains("manta_track_rail_ratio_count 6039\n"));
        assert!(text.contains(r#"manta_candidate_repetitions_bucket{le="1"} 116"#));
        assert!(text.contains(r#"manta_candidate_repetitions_bucket{le="2"} 154"#));
        assert!(text.contains(r#"manta_candidate_repetitions_bucket{le="+Inf"} 274"#));
        assert!(text.contains("manta_candidate_repetitions_sum 899\n"));
        assert!(text.contains("manta_candidate_repetitions_count 274\n"));
    }

    #[test]
    fn decode_quality_renders_between_decode_latency_and_healthy() {
        let m = Metrics::new();
        m.set_decode_quality(sample_decode_quality());
        let text = m.render_prometheus_text();
        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle}"));
        let latency = at("# TYPE manta_decode_latency_seconds histogram");
        let chars = at("# TYPE manta_decode_chars_total counter");
        let reps = at("# TYPE manta_candidate_repetitions histogram");
        let healthy = at("# TYPE manta_healthy gauge");
        assert!(latency < chars && chars < reps && reps < healthy);
    }

    #[test]
    fn decode_quality_does_not_feed_the_decode_watchdog() {
        let m = Metrics::new();
        let now = Instant::now();
        m.arm_decode_watchdog_at(now);
        let before = m.health_at(now);
        m.set_decode_quality(sample_decode_quality());
        assert_eq!(m.health_at(now), before);
    }

    #[test]
    fn decode_latency_family_header_only_when_never_set() {
        let text = Metrics::new().render_prometheus_text();
        assert!(text.contains("# TYPE manta_decode_latency_seconds histogram"));
        assert!(!text.contains("manta_decode_latency_seconds_bucket{"));
        assert!(!text.contains("manta_decode_latency_seconds_count"));
    }

    #[test]
    fn decode_latency_samples_are_contiguous_and_bucket_ordered() {
        let m = Metrics::new();
        m.set_decode_latency(LatencyHistogram {
            bounds_seconds: vec![0.001, 0.01, 0.1],
            bucket_counts: vec![1, 1, 1, 1],
            sum_seconds: 0.05,
        });
        let text = m.render_prometheus_text();
        let b1 = text.find(r#"le="0.001""#).unwrap();
        let b2 = text.find(r#"le="0.01""#).unwrap();
        let b3 = text.find(r#"le="0.1""#).unwrap();
        let binf = text.find(r#"le="+Inf""#).unwrap();
        let sum = text.find("manta_decode_latency_seconds_sum").unwrap();
        let count = text.find("manta_decode_latency_seconds_count").unwrap();
        assert!(b1 < b2 && b2 < b3 && b3 < binf && binf < sum && sum < count);
    }

    // MAN-128: health evaluation (`/healthz`'s backing state).

    #[test]
    fn healthy_when_source_listeners_and_decode_all_ok() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        m.set_listener_up("json", true);
        m.set_listener_up("metrics", true);
        let t0 = Instant::now();
        m.arm_decode_watchdog_at(t0);
        m.set_decode_latency_at(
            LatencyHistogram {
                bounds_seconds: vec![0.001],
                bucket_counts: vec![5, 0],
                sum_seconds: 0.001,
            },
            t0 + Duration::from_secs(1),
        );
        assert!(m.health_at(t0 + Duration::from_secs(2)).healthy);
    }

    #[test]
    fn unhealthy_when_any_source_unhealthy() {
        let m = Metrics::new();
        m.set_source_health("file", false);
        m.set_listener_up("telnet", true);
        assert!(!m.health_at(Instant::now()).healthy);
    }

    #[test]
    fn unhealthy_when_no_source_registered() {
        let m = Metrics::new();
        m.set_listener_up("telnet", true);
        assert!(!m.health_at(Instant::now()).healthy);
    }

    #[test]
    fn unhealthy_when_a_listener_is_down() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        m.set_listener_up("json", false);
        assert!(!m.health_at(Instant::now()).healthy);
    }

    #[test]
    fn unhealthy_when_no_listener_registered() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        assert!(!m.health_at(Instant::now()).healthy);
    }

    #[test]
    fn unhealthy_when_decode_progress_stalls_past_threshold() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let t0 = Instant::now();
        m.arm_decode_watchdog_at(t0);
        m.set_decode_latency_at(
            LatencyHistogram {
                bounds_seconds: vec![0.001],
                bucket_counts: vec![1, 0],
                sum_seconds: 0.001,
            },
            t0 + Duration::from_secs(1),
        );
        let report = m.health_at(
            t0 + Duration::from_secs(1)
                + crate::health::DECODE_STALL_THRESHOLD
                + Duration::from_millis(1),
        );
        assert!(!report.healthy);
        assert!(report.render_text().contains("stalled"));
    }

    #[test]
    fn progress_requires_count_to_increase_not_just_a_snapshot() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let t0 = Instant::now();
        m.arm_decode_watchdog_at(t0);
        let same = LatencyHistogram {
            bounds_seconds: vec![0.001],
            bucket_counts: vec![1, 0],
            sum_seconds: 0.001,
        };
        m.set_decode_latency_at(same.clone(), t0 + Duration::from_secs(1));
        m.set_decode_latency_at(same, t0 + Duration::from_secs(5));
        let report = m.health_at(
            t0 + Duration::from_secs(1)
                + crate::health::DECODE_STALL_THRESHOLD
                + Duration::from_millis(1),
        );
        assert!(
            !report.healthy,
            "an unchanged count must not refresh progress"
        );
    }

    #[test]
    fn unhealthy_once_decode_marked_stopped_even_if_recent_progress() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let t0 = Instant::now();
        m.arm_decode_watchdog_at(t0);
        m.set_decode_latency_at(
            LatencyHistogram {
                bounds_seconds: vec![0.001],
                bucket_counts: vec![1, 0],
                sum_seconds: 0.001,
            },
            t0,
        );
        m.mark_decode_stopped();
        assert!(!m.health_at(t0 + Duration::from_millis(1)).healthy);
    }

    #[test]
    fn startup_grace_counts_from_arming() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let t0 = Instant::now();
        m.arm_decode_watchdog_at(t0);
        assert!(m.health_at(t0 + Duration::from_secs(5)).healthy);
        assert!(
            !m.health_at(t0 + crate::health::DECODE_STALL_THRESHOLD + Duration::from_secs(1))
                .healthy
        );
    }

    #[test]
    fn decode_check_skipped_when_watchdog_never_armed() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let report = m.health_at(Instant::now());
        assert!(report.healthy);
        assert!(report.render_text().contains("not monitored"));
    }

    #[test]
    fn health_report_text_lists_every_check_one_per_line() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        let text = m.health_at(Instant::now()).render_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "ok");
        assert!(lines.contains(&"source file: healthy"));
        assert!(lines.contains(&"listener telnet: up"));
    }

    #[test]
    fn manta_healthy_and_listener_up_gauges_match_health_at() {
        let m = Metrics::new();
        m.set_source_health("file", true);
        m.set_listener_up("telnet", true);
        m.set_listener_up("json", false);
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_healthy 0"));
        assert!(text.contains(r#"manta_listener_up{listener="json"} 0"#));
    }

    // MAN-44: reconnect window + health classification.

    #[test]
    fn reconnects_outside_the_window_do_not_count_as_recent() {
        let m = Metrics::new();
        let t = m.register_uplink_target("a.example:7000".into(), true);
        let t0 = Instant::now();

        t.record_reconnect_at(t0);
        t.record_reconnect_at(t0 + Duration::from_secs(10));
        assert_eq!(
            t.snapshot_at(t0 + Duration::from_secs(20))
                .recent_reconnects,
            2
        );
        // Cumulative never decays; the window does.
        let later = t0 + Duration::from_secs(10) + RECONNECT_WINDOW + Duration::from_secs(1);
        assert_eq!(t.snapshot_at(later).recent_reconnects, 0);
        assert_eq!(t.snapshot_at(later).reconnects, 2);
    }

    #[test]
    fn recent_reconnect_ring_is_bounded_under_sustained_flapping() {
        let m = Metrics::new();
        let t = m.register_uplink_target("a.example:7000".into(), true);
        let t0 = Instant::now();
        for i in 0..10_000u64 {
            t.record_reconnect_at(t0 + Duration::from_millis(i));
        }
        assert!(t.recent_len() <= MAX_TRACKED_RECONNECTS);
        assert_eq!(
            t.snapshot_at(t0 + Duration::from_secs(1)).reconnects,
            10_000
        );
    }

    #[test]
    fn health_classifies_disabled_flapping_connected_and_down_in_that_priority() {
        assert_eq!(
            classify_uplink_health(false, true, 0),
            UplinkHealth::Disabled
        );
        // Flapping outranks connected: reconnecting every 60s while
        // momentarily up is not healthy.
        assert_eq!(
            classify_uplink_health(true, true, FLAPPING_RECONNECTS),
            UplinkHealth::Flapping
        );
        assert_eq!(
            classify_uplink_health(true, false, FLAPPING_RECONNECTS),
            UplinkHealth::Flapping
        );
        assert_eq!(
            classify_uplink_health(true, true, FLAPPING_RECONNECTS - 1),
            UplinkHealth::Connected
        );
        assert_eq!(classify_uplink_health(true, false, 0), UplinkHealth::Down);
    }

    #[test]
    fn overall_uplink_health_is_ok_only_when_every_enabled_target_is_connected() {
        let m = Metrics::new();
        let a = m.register_uplink_target("a.example:7000".into(), true);
        let b = m.register_uplink_target("b.example:7000".into(), true);
        let _disabled = m.register_uplink_target("c.example:7000".into(), false);
        a.mark_connected();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Degraded);
        b.mark_connected();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Ok);
    }

    #[test]
    fn overall_uplink_health_is_disabled_when_no_targets_are_enabled() {
        let m = Metrics::new();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Disabled);
        let _disabled = m.register_uplink_target("a.example:7000".into(), false);
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Disabled);
    }

    #[test]
    fn overall_uplink_health_is_down_when_every_enabled_target_is_down() {
        let m = Metrics::new();
        let _a = m.register_uplink_target("a.example:7000".into(), true);
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Down);
    }

    #[test]
    fn a_registered_but_never_connected_target_still_appears_in_snapshots() {
        let m = Metrics::new();
        let _ = m.register_uplink_target("never.example:7000".into(), true);
        let snap = m.uplink_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].label, "never.example:7000");
        assert_eq!(snap[0].health, UplinkHealth::Down);
    }

    #[test]
    fn renders_the_recent_reconnects_gauge_per_target() {
        let m = Metrics::new();
        let t = m.register_uplink_target("a.example:7000".into(), true);
        t.record_reconnect();
        t.record_reconnect();
        let text = m.render_prometheus_text();
        assert!(
            text.contains(r#"manta_uplink_target_recent_reconnects{target="a.example:7000"} 2"#)
        );
    }

    // ---- MAN-301: manta_rbn_compare_* ----

    use crate::rbn_compare::grade::{Disagreement, Grade, SpotGrade};

    fn spot_grade(band: &'static str, grade: Grade, offset: Option<i64>) -> SpotGrade {
        SpotGrade {
            kind: "spotGrade",
            spot_id: "W3XYZ-1:1:1:1:W1AW".to_string(),
            call: "W1AW".to_string(),
            band,
            spot_time: 1_791_720_000,
            window_close: 1_791_720_180,
            window_s: 180,
            grade,
            disagreements: Vec::new(),
            manta_freq_hz: 14_025_100,
            manta_wpm: Some(25),
            rbn_modal_freq_hz: offset.map(|o| 14_025_100 - o),
            rbn_median_wpm: offset.map(|_| 24.0),
            rbn_skimmers: u32::from(offset.is_some()),
            rbn_reports: u32::from(offset.is_some()),
            freq_offset_hz: offset,
            rbn_bands: Vec::new(),
            spotter_filter: Vec::new(),
        }
    }

    fn rbn_line_value(text: &str, series: &str) -> Option<String> {
        text.lines().find_map(|l| {
            l.strip_prefix(series)?
                .strip_prefix(' ')
                .map(str::to_string)
        })
    }

    #[test]
    fn rbn_compare_families_absent_until_enabled() {
        let m = Metrics::new();
        m.record_rbn_grade(&spot_grade("20m", Grade::Match, Some(40)));
        m.set_rbn_feed_connected(true);
        m.inc_rbn_feed_reports();
        m.inc_rbn_feed_reconnects();
        m.set_rbn_pending(3);
        m.add_rbn_ungraded(2);
        assert!(
            !m.render_prometheus_text().contains("manta_rbn_compare_"),
            "no rbn_compare family before enable_rbn_compare"
        );

        m.enable_rbn_compare();
        let text = m.render_prometheus_text();
        for family in [
            "manta_rbn_compare_grades_total",
            "manta_rbn_compare_grades_last_hour",
            "manta_rbn_compare_disagreements_last_hour",
            "manta_rbn_compare_freq_offset_median_hz",
            "manta_rbn_compare_freq_offset_samples",
            "manta_rbn_compare_feed_connected",
            "manta_rbn_compare_feed_reports_total",
            "manta_rbn_compare_feed_reconnects_total",
            "manta_rbn_compare_pending",
            "manta_rbn_compare_ungraded_total",
        ] {
            assert!(text.contains(&format!("# TYPE {family} ")), "{family}");
        }
        // Calls made before enabling were not recorded.
        assert_eq!(
            rbn_line_value(&text, "manta_rbn_compare_feed_reports_total").as_deref(),
            Some("0")
        );
        assert!(!text.contains("manta_rbn_compare_grades_total{"));
    }

    #[test]
    fn hundred_grades_render_per_band_last_hour_counts() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        let plan = [
            ("20m", Grade::Match, 52),
            ("20m", Grade::Disagree, 6),
            ("20m", Grade::Elsewhere, 2),
            ("20m", Grade::None, 9),
            ("40m", Grade::Match, 23),
            ("40m", Grade::None, 8),
        ];
        for (band, grade, n) in plan {
            for _ in 0..n {
                m.record_rbn_grade_at(&spot_grade(band, grade, Some(0)), t0);
            }
        }
        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0 + Duration::from_secs(10));

        let mut last_hour_sum = 0u64;
        let mut total_sum = 0u64;
        for (band, grade, n) in plan {
            let label = grade_label(grade);
            let last = rbn_line_value(
                &out,
                &format!("manta_rbn_compare_grades_last_hour{{band=\"{band}\",grade=\"{label}\"}}"),
            )
            .unwrap_or_else(|| panic!("{band}/{label} missing:\n{out}"));
            assert_eq!(last, n.to_string(), "{band}/{label}");
            last_hour_sum += last.parse::<u64>().unwrap();
            let total = rbn_line_value(
                &out,
                &format!("manta_rbn_compare_grades_total{{band=\"{band}\",grade=\"{label}\"}}"),
            )
            .unwrap();
            assert_eq!(total, n.to_string(), "{band}/{label} total");
            total_sum += total.parse::<u64>().unwrap();
        }
        assert_eq!(last_hour_sum, 100);
        assert_eq!(total_sum, 100);
        assert_eq!(
            out.lines()
                .filter(|l| l.starts_with("manta_rbn_compare_grades_last_hour{"))
                .count(),
            6
        );
    }

    #[test]
    fn grades_older_than_an_hour_leave_the_last_hour_gauge_but_not_the_total() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        m.record_rbn_grade_at(&spot_grade("20m", Grade::Match, Some(10)), t0);
        m.record_rbn_grade_at(
            &spot_grade("20m", Grade::Match, Some(30)),
            t0 + Duration::from_secs(1800),
        );

        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0 + GRADE_WINDOW + Duration::from_secs(1));
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_last_hour{band="20m",grade="match"}"#
            )
            .as_deref(),
            Some("1")
        );
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_total{band="20m",grade="match"}"#
            )
            .as_deref(),
            Some("2")
        );
        // The expired grade left the median too.
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_freq_offset_median_hz{band="20m"}"#
            )
            .as_deref(),
            Some("30")
        );
        assert_eq!(
            rbn_line_value(&out, r#"manta_rbn_compare_freq_offset_samples{band="20m"}"#).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn last_hour_series_stays_at_zero_once_seen() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        let mut g = spot_grade("40m", Grade::Disagree, Some(900));
        g.disagreements = vec![Disagreement::Frequency];
        m.record_rbn_grade_at(&g, t0);

        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0 + GRADE_WINDOW * 2);
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_last_hour{band="40m",grade="disagree"}"#
            )
            .as_deref(),
            Some("0")
        );
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_disagreements_last_hour{band="40m",kind="frequency"}"#
            )
            .as_deref(),
            Some("0")
        );
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_total{band="40m",grade="disagree"}"#
            )
            .as_deref(),
            Some("1")
        );
        // A median over no grades is absent, not zero.
        assert!(!out.contains(r#"manta_rbn_compare_freq_offset_median_hz{band="40m"}"#));
        assert!(!out.contains(r#"manta_rbn_compare_freq_offset_samples{band="40m"}"#));
    }

    #[test]
    fn median_offset_is_per_band_over_match_and_disagree_only() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        // 20m, odd count over match + disagree: {-10, 40, 50} -> 40.
        m.record_rbn_grade_at(&spot_grade("20m", Grade::Match, Some(50)), t0);
        m.record_rbn_grade_at(&spot_grade("20m", Grade::Match, Some(-10)), t0);
        let mut d = spot_grade("20m", Grade::Disagree, Some(40));
        d.disagreements = vec![Disagreement::Speed];
        m.record_rbn_grade_at(&d, t0);
        // Offsets carried by other grades never count.
        m.record_rbn_grade_at(&spot_grade("20m", Grade::Elsewhere, Some(9_999)), t0);
        m.record_rbn_grade_at(&spot_grade("20m", Grade::Unknown, Some(-9_999)), t0);
        // 40m, even count: {-30, -20, -10, 15} -> -15.
        for o in [-20, -10, 15, -30] {
            m.record_rbn_grade_at(&spot_grade("40m", Grade::Match, Some(o)), t0);
        }
        // 80m: only `none` grades -> no median.
        m.record_rbn_grade_at(&spot_grade("80m", Grade::None, None), t0);
        // 15m, even count with a half-Hz median: {10, 11} -> 10.5.
        m.record_rbn_grade_at(&spot_grade("15m", Grade::Match, Some(11)), t0);
        m.record_rbn_grade_at(&spot_grade("15m", Grade::Match, Some(10)), t0);

        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0);
        let median = |band: &str| {
            rbn_line_value(
                &out,
                &format!("manta_rbn_compare_freq_offset_median_hz{{band=\"{band}\"}}"),
            )
        };
        let samples = |band: &str| {
            rbn_line_value(
                &out,
                &format!("manta_rbn_compare_freq_offset_samples{{band=\"{band}\"}}"),
            )
        };
        assert_eq!(median("20m").as_deref(), Some("40"));
        assert_eq!(samples("20m").as_deref(), Some("3"));
        assert_eq!(median("40m").as_deref(), Some("-15"));
        assert_eq!(samples("40m").as_deref(), Some("4"));
        assert_eq!(median("15m").as_deref(), Some("10.5"));
        assert_eq!(samples("15m").as_deref(), Some("2"));
        assert_eq!(median("80m"), None);
        assert_eq!(samples("80m"), None);
    }

    #[test]
    fn disagreement_kinds_count_separately() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        let mut both = spot_grade("20m", Grade::Disagree, Some(1400));
        both.disagreements = vec![Disagreement::Frequency, Disagreement::Speed];
        m.record_rbn_grade_at(&both, t0);
        let mut speed = spot_grade("20m", Grade::Disagree, Some(0));
        speed.disagreements = vec![Disagreement::Speed];
        m.record_rbn_grade_at(&speed, t0);

        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0);
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_disagreements_last_hour{band="20m",kind="frequency"}"#
            )
            .as_deref(),
            Some("1")
        );
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_disagreements_last_hour{band="20m",kind="speed"}"#
            )
            .as_deref(),
            Some("2")
        );
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_last_hour{band="20m",grade="disagree"}"#
            )
            .as_deref(),
            Some("2"),
            "one spot with both kinds is still one disagree grade"
        );
    }

    #[test]
    fn recent_rings_are_bounded() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let t0 = Instant::now();
        let mut g = spot_grade("20m", Grade::Disagree, Some(5));
        g.disagreements = vec![Disagreement::Frequency];
        for _ in 0..(MAX_RECENT_GRADES + 50) {
            m.record_rbn_grade_at(&g, t0);
        }
        {
            let rc = m.rbn_compare.get().unwrap();
            let book = rc.grades.lock().unwrap();
            assert_eq!(book.recent[&("20m", "disagree")].len(), MAX_RECENT_GRADES);
            assert_eq!(
                book.disagreements[&("20m", "frequency")].len(),
                MAX_RECENT_GRADES
            );
            assert_eq!(book.offsets["20m"].len(), MAX_RECENT_GRADES);
        }
        let mut out = String::new();
        m.render_rbn_compare_at(&mut out, t0);
        assert_eq!(
            rbn_line_value(
                &out,
                r#"manta_rbn_compare_grades_total{band="20m",grade="disagree"}"#
            ),
            Some((MAX_RECENT_GRADES + 50).to_string()),
            "the cumulative counter stays exact past the cap"
        );
    }

    #[test]
    fn feed_and_pending_gauges_render() {
        let m = Metrics::new();
        m.enable_rbn_compare();
        let text = m.render_prometheus_text();
        assert_eq!(
            rbn_line_value(&text, "manta_rbn_compare_feed_connected").as_deref(),
            Some("0")
        );

        m.set_rbn_feed_connected(true);
        for _ in 0..7 {
            m.inc_rbn_feed_reports();
        }
        m.inc_rbn_feed_reconnects();
        m.inc_rbn_feed_reconnects();
        m.set_rbn_pending(12);
        m.add_rbn_ungraded(3);
        m.add_rbn_ungraded(4);
        let text = m.render_prometheus_text();
        for (series, value) in [
            ("manta_rbn_compare_feed_connected", "1"),
            ("manta_rbn_compare_feed_reports_total", "7"),
            ("manta_rbn_compare_feed_reconnects_total", "2"),
            ("manta_rbn_compare_pending", "12"),
            ("manta_rbn_compare_ungraded_total", "7"),
        ] {
            assert_eq!(
                rbn_line_value(&text, series).as_deref(),
                Some(value),
                "{series}"
            );
        }

        m.set_rbn_feed_connected(false);
        m.set_rbn_pending(0);
        let text = m.render_prometheus_text();
        assert_eq!(
            rbn_line_value(&text, "manta_rbn_compare_feed_connected").as_deref(),
            Some("0")
        );
        assert_eq!(
            rbn_line_value(&text, "manta_rbn_compare_pending").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn history_truncation_is_counted_and_rendered() {
        let metrics = Metrics::new();
        assert_eq!(metrics.spots_history_truncated_total(), 0);
        metrics.record_history_truncated(2);
        metrics.record_history_truncated(3);
        assert_eq!(metrics.spots_history_truncated_total(), 5);
        let text = metrics.render_prometheus_text();
        assert!(text.contains("# HELP manta_spots_history_truncated_total Spots dropped from the sh/dx history at its 10000-spot ceiling while still inside [server].history_minutes. Non-zero means sh/dx reaches back less than the configured window.\n"));
        assert!(text.contains("# TYPE manta_spots_history_truncated_total counter\n"));
        assert!(text.contains("manta_spots_history_truncated_total 5\n"));
    }

    // MAN-316: reference-list families.

    const SHA_A: &str = "e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1";
    const SHA_B: &str = "75aaca91f62bd010b32d3e98193e9abe2330a640d9edf9a0a58b4d628ef1aa6c";

    /// The sample lines of one family, in render order.
    fn samples<'a>(text: &'a str, family: &str) -> Vec<&'a str> {
        text.lines()
            .filter(|l| l.starts_with(&format!("{family}{{")))
            .collect()
    }

    #[test]
    fn renders_list_info_per_list() {
        let m = Metrics::new();
        m.set_list_in_use("scp", SHA_B, ListOrigin::Bundled);
        m.set_list_in_use("cty", SHA_A, ListOrigin::Cache);
        let text = m.render_prometheus_text();
        assert_eq!(
            samples(&text, "manta_list_info"),
            [
                format!(r#"manta_list_info{{list="cty",sha256="{SHA_A}",origin="cache"}} 1"#),
                format!(r#"manta_list_info{{list="scp",sha256="{SHA_B}",origin="bundled"}} 1"#),
            ]
        );
    }

    #[test]
    fn list_info_replaces_the_series_on_swap() {
        let m = Metrics::new();
        m.set_list_in_use("cty", SHA_A, ListOrigin::Bundled);
        m.set_list_in_use("cty", SHA_B, ListOrigin::Download);
        let text = m.render_prometheus_text();
        assert_eq!(
            samples(&text, "manta_list_info"),
            [format!(
                r#"manta_list_info{{list="cty",sha256="{SHA_B}",origin="download"}} 1"#
            )]
        );
        assert!(!text.contains(SHA_A), "{text}");
    }

    #[test]
    fn renders_list_refresh_outcomes_from_zero_once_enabled() {
        let m = Metrics::new();
        m.enable_list_refresh("scp");
        assert_eq!(
            samples(&m.render_prometheus_text(), "manta_list_refresh_total"),
            [
                r#"manta_list_refresh_total{list="scp",outcome="updated"} 0"#,
                r#"manta_list_refresh_total{list="scp",outcome="unchanged"} 0"#,
                r#"manta_list_refresh_total{list="scp",outcome="failed"} 0"#,
            ]
        );

        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_795_600);
        m.record_list_refresh("scp", ListRefreshOutcome::Failed, at);
        m.record_list_refresh("scp", ListRefreshOutcome::Failed, at);
        m.record_list_refresh("scp", ListRefreshOutcome::Unchanged, at);
        m.enable_list_refresh("scp"); // idempotent: keeps the counts
        m.enable_list_refresh("cty");
        m.record_list_refresh("cty", ListRefreshOutcome::Updated, at);
        assert_eq!(
            samples(&m.render_prometheus_text(), "manta_list_refresh_total"),
            [
                r#"manta_list_refresh_total{list="cty",outcome="updated"} 1"#,
                r#"manta_list_refresh_total{list="cty",outcome="unchanged"} 0"#,
                r#"manta_list_refresh_total{list="cty",outcome="failed"} 0"#,
                r#"manta_list_refresh_total{list="scp",outcome="updated"} 0"#,
                r#"manta_list_refresh_total{list="scp",outcome="unchanged"} 1"#,
                r#"manta_list_refresh_total{list="scp",outcome="failed"} 2"#,
            ]
        );
    }

    #[test]
    fn list_refresh_families_render_help_and_type_without_samples_when_unused() {
        let m = Metrics::new();
        // A list in use but not refreshed (refresh_lists off) has an info
        // series and nothing else.
        m.set_list_in_use("cty", SHA_A, ListOrigin::Bundled);
        let text = m.render_prometheus_text();
        for (family, kind, help) in [
            (
                "manta_list_info",
                "gauge",
                "The reference list in use: list (cty or scp), the SHA-256 of its bytes, and its origin (bundled, file, cache or download). Always 1.",
            ),
            (
                "manta_list_refresh_total",
                "counter",
                "Reference-list download attempts by outcome: updated, unchanged or failed.",
            ),
            (
                "manta_list_last_success_timestamp_seconds",
                "gauge",
                "Unix time the reference list in use was last confirmed current with its download URL.",
            ),
        ] {
            assert!(text.contains(&format!("# HELP {family} {help}\n")), "{family}");
            assert!(text.contains(&format!("# TYPE {family} {kind}\n")), "{family}");
        }
        assert!(samples(&text, "manta_list_refresh_total").is_empty());
        assert!(samples(&text, "manta_list_last_success_timestamp_seconds").is_empty());

        let empty = Metrics::new().render_prometheus_text();
        assert!(empty.contains("# TYPE manta_list_info gauge\n"));
        assert!(samples(&empty, "manta_list_info").is_empty());
    }

    #[test]
    fn renders_last_success_timestamp_only_after_a_success() {
        let m = Metrics::new();
        m.enable_list_refresh("cty");
        m.enable_list_refresh("scp");
        let failed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_700_000);
        m.record_list_refresh("cty", ListRefreshOutcome::Failed, failed_at);
        let text = m.render_prometheus_text();
        assert!(
            samples(&text, "manta_list_last_success_timestamp_seconds").is_empty(),
            "{text}"
        );

        // Rendered in whole seconds: the sub-second part is truncated.
        let ok_at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_795_600_750);
        m.record_list_refresh("cty", ListRefreshOutcome::Updated, ok_at);
        // The cache's checked_at at startup counts as a confirmation too.
        let checked_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_600_000);
        m.set_list_confirmed_at("scp", checked_at);
        assert_eq!(
            samples(
                &m.render_prometheus_text(),
                "manta_list_last_success_timestamp_seconds"
            ),
            [
                r#"manta_list_last_success_timestamp_seconds{list="cty"} 1791795600"#,
                r#"manta_list_last_success_timestamp_seconds{list="scp"} 1791600000"#,
            ]
        );

        // A later failure leaves the last success alone.
        m.record_list_refresh(
            "cty",
            ListRefreshOutcome::Failed,
            ok_at + Duration::from_secs(60),
        );
        assert!(m
            .render_prometheus_text()
            .contains(r#"manta_list_last_success_timestamp_seconds{list="cty"} 1791795600"#));
    }
}
