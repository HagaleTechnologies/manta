//! Prometheus text-format metrics, plus the per-target RBN uplink health
//! registry `status.rs`'s `GET /status` document and `manta status` are
//! built on (MAN-44). ARCHITECTURE §8: "manta status ... GET /status on
//! the metrics listener... Prometheus text endpoint... spot rate, active
//! tracks..."; MAN-12 scenario 3 ("operators can inspect health without
//! reading source"). `Metrics` owns what `manta-server` genuinely knows
//! (spots published, connected clients per protocol, per-target uplink
//! state) and exposes `set_active_tracks`/`set_source_health` for the
//! daemon wiring layer to inject engine-owned numbers manta-server has no
//! way to compute itself.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Rolling window over which "recent" reconnects are counted (MAN-44).
/// Matches the ticket's "reconnecting repeatedly over several minutes".
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(300);
/// Recent reconnects at or above this count read as a stuck reconnect
/// loop. `uplink::MAX_BACKOFF` is 60s, so a target that is simply down
/// produces ~5 reconnects per window -- comfortably over this -- while a
/// single transient blip (whose backoff then resets to
/// `uplink::INITIAL_BACKOFF`) does not trip it.
pub const FLAPPING_RECONNECTS: u32 = 3;
/// Hard cap on retained reconnect timestamps per target. A target
/// flapping far faster than the window would otherwise grow this
/// unbounded; the cumulative reconnect counter stays exact regardless,
/// only `recent_reconnects` saturates. Same bounded-cardinality
/// discipline as MAN-62's `OccurrenceTracker` (see `bus.rs`).
const MAX_TRACKED_RECONNECTS: usize = 256;

/// Immutable description of one configured `[[rbn_uplink]]` target
/// (MAN-44). Plain data so `metrics` needs no dependency on `config` --
/// `uplink::target_specs` builds these from `RbnUplinkConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkTargetSpec {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    pub dry_run: bool,
}

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
/// needs per-target rows (`status::StatusDoc::from_metrics`) can derive
/// both from the SAME snapshot at the SAME `now` instead of walking the
/// registry twice at two different instants (MAN-44 code review CR-3) --
/// which is also what previously let `connected_targets` (raw
/// `connected` bool) disagree with this verdict (`health ==
/// UplinkHealth::Connected`) for a target that is flapping but
/// momentarily connected (CR-1).
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

/// One configured uplink target's live state (MAN-44). Handed to that
/// target's `uplink::serve` task, which is the ONLY writer -- every
/// counter here has a real call site in `uplink.rs`, deliberately unlike
/// `active_tracks` (ARCHITECTURE.md's "served but never populated"
/// caution), which stays frozen at its initial value in production.
///
/// Replaces MAN-32/MAN-42's single shared `uplink_connected_count`
/// last-writer-wins-prone atomic: each target now owns its own
/// `AtomicBool`, so one target's failed reconnect can never clear
/// another's connected reading even under an unbalanced call (the
/// structural fix for the round-1 MAN-42 review finding -- previously
/// only a testing/calling discipline).
pub struct UplinkTarget {
    spec: UplinkTargetSpec,
    connected: AtomicBool,
    sent: AtomicU64,
    suppressed: AtomicU64,
    lagged: AtomicU64,
    write_failed: AtomicU64,
    disconnected: AtomicU64,
    reconnects: AtomicU64,
    recent: Mutex<VecDeque<Instant>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UplinkTargetSnapshot {
    #[serde(rename = "target")]
    pub label: String,
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    pub dry_run: bool,
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

impl UplinkTarget {
    pub fn record_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_suppressed(&self) {
        self.suppressed.fetch_add(1, Ordering::Relaxed);
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

    /// Call exactly once per connect transition, paired with exactly one
    /// `mark_disconnected` -- same contract as MAN-32's original
    /// `Metrics::mark_uplink_connected`, but now per target, so an
    /// unbalanced call can never desync any OTHER target's reading
    /// (MAN-42's round-1 bug class).
    pub fn mark_connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
    }

    /// See `mark_connected` -- call only to undo a prior `mark_connected`
    /// from this same target's same connection.
    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    pub fn record_reconnect(&self) {
        self.record_reconnect_at(Instant::now());
    }

    /// `_at` variant so the reconnect window is testable without sleeping
    /// or a paused runtime; `record_reconnect` above is the only
    /// production caller.
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

    pub fn snapshot(&self) -> UplinkTargetSnapshot {
        self.snapshot_at(Instant::now())
    }

    /// `_at` variant so callers (`Metrics::uplink_snapshot_at`) can render
    /// a whole registry's worth of targets against one consistent `now`,
    /// and so window behavior stays testable without sleeping.
    pub fn snapshot_at(&self, now: Instant) -> UplinkTargetSnapshot {
        let recent_reconnects = {
            let mut recent = self
                .recent
                .lock()
                .expect("uplink recent-reconnects lock poisoned");
            prune_reconnects(&mut recent, now);
            recent.len() as u32
        };
        let connected = self.connected.load(Ordering::Relaxed);
        UplinkTargetSnapshot {
            label: self.spec.label.clone(),
            host: self.spec.host.clone(),
            port: self.spec.port,
            enabled: self.spec.enabled,
            dry_run: self.spec.dry_run,
            connected,
            sent: self.sent.load(Ordering::Relaxed),
            suppressed: self.suppressed.load(Ordering::Relaxed),
            lagged: self.lagged.load(Ordering::Relaxed),
            write_failed: self.write_failed.load(Ordering::Relaxed),
            disconnected: self.disconnected.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            recent_reconnects,
            health: classify_uplink_health(self.spec.enabled, connected, recent_reconnects),
        }
    }
}

/// Escapes a Prometheus label value per the text-exposition-format spec
/// (backslash, double-quote, newline) -- MAN-44: an uplink target's
/// `host:port` label comes from operator config, not a validated grammar
/// (unlike a callsign), so an unescaped value could otherwise produce a
/// malformed exposition line. Applied to `source` below too (a one-line
/// correctness fix made while already touching this rendering code): that
/// label is also an operator-supplied string and was previously
/// unescaped.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub struct Metrics {
    spots_total: AtomicU64,
    spots_dropped_lagged_total: AtomicU64,
    spots_suppressed_by_filter_total: AtomicU64,
    spots_dropped_write_failed_total: AtomicU64,
    telnet_clients: AtomicI64,
    json_clients: AtomicI64,
    ws_clients: AtomicI64,
    active_tracks: AtomicU64,
    source_health: RwLock<BTreeMap<String, bool>>,
    /// Registered uplink targets, in config order (MAN-44). Written once
    /// per target at daemon wiring time (`start_spot_server`'s call to
    /// `register_uplink_target`), read on every render/status snapshot.
    /// Every aggregate uplink figure (`uplink_sent_total` etc.) is now
    /// DERIVED by summing over this registry rather than maintained as a
    /// separate counter -- two independently incremented sources of the
    /// same number is exactly how MAN-42's round-1 last-writer-wins bug
    /// happened; deriving removes the class entirely.
    uplink_targets: RwLock<Vec<Arc<UplinkTarget>>>,
    started_at: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            spots_total: AtomicU64::new(0),
            spots_dropped_lagged_total: AtomicU64::new(0),
            spots_suppressed_by_filter_total: AtomicU64::new(0),
            spots_dropped_write_failed_total: AtomicU64::new(0),
            telnet_clients: AtomicI64::new(0),
            json_clients: AtomicI64::new(0),
            ws_clients: AtomicI64::new(0),
            active_tracks: AtomicU64::new(0),
            source_health: RwLock::new(BTreeMap::new()),
            uplink_targets: RwLock::new(Vec::new()),
            started_at: Instant::now(),
        }
    }

    pub fn record_spot(&self) {
        self.spots_total.fetch_add(1, Ordering::Relaxed);
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

    pub fn telnet_clients(&self) -> i64 {
        self.telnet_clients.load(Ordering::Relaxed)
    }

    pub fn json_clients(&self) -> i64 {
        self.json_clients.load(Ordering::Relaxed)
    }

    pub fn ws_clients(&self) -> i64 {
        self.ws_clients.load(Ordering::Relaxed)
    }

    pub fn spots_total(&self) -> u64 {
        self.spots_total.load(Ordering::Relaxed)
    }

    /// Engine-owned figure, injected by the daemon wiring layer (see
    /// module doc) -- `manta-server` has no track manager of its own.
    /// `None` until a real call site sets it (ARCHITECTURE.md: no
    /// production caller exists yet) -- `status.rs` renders that as
    /// "n/a", not a misleading live-looking `0`.
    pub fn set_active_tracks(&self, count: u64) {
        self.active_tracks.store(count, Ordering::Relaxed);
    }

    pub fn active_tracks(&self) -> u64 {
        self.active_tracks.load(Ordering::Relaxed)
    }

    pub fn set_source_health(&self, source: &str, healthy: bool) {
        self.source_health
            .write()
            .expect("source_health lock poisoned")
            .insert(source.to_string(), healthy);
    }

    /// Wall-clock time since this `Metrics` (and so the daemon) started
    /// (MAN-44) -- `status.rs` renders this as `manta status`'s "daemon
    /// up ..." line.
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    // MAN-44: per-target uplink registry.

    /// Registers a newly configured uplink target, returning the handle
    /// its `uplink::serve` task owns and writes to for its whole
    /// lifetime. Registered once per `[[rbn_uplink]]` entry at daemon
    /// wiring time (`start_spot_server`), in config order -- including
    /// disabled entries, so `manta status` shows "configured but off"
    /// rather than silently omitting them (a target that never manages a
    /// single successful connection must still be visible).
    pub fn register_uplink_target(&self, spec: UplinkTargetSpec) -> Arc<UplinkTarget> {
        let target = Arc::new(UplinkTarget {
            spec,
            connected: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            write_failed: AtomicU64::new(0),
            disconnected: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        });
        self.uplink_targets
            .write()
            .expect("uplink_targets lock poisoned")
            .push(target.clone());
        target
    }

    pub fn uplink_snapshot(&self) -> Vec<UplinkTargetSnapshot> {
        self.uplink_snapshot_at(Instant::now())
    }

    pub fn uplink_snapshot_at(&self, now: Instant) -> Vec<UplinkTargetSnapshot> {
        self.uplink_targets
            .read()
            .expect("uplink_targets lock poisoned")
            .iter()
            .map(|t| t.snapshot_at(now))
            .collect()
    }

    /// `Ok` only when every ENABLED target's own health classifies as
    /// `Connected` (MAN-44 decision 5) -- a flapping target counts the
    /// same as a down one here, since it being momentarily connected
    /// whenever observed is exactly the state this ticket exists to make
    /// visible, not paper over. `Disabled` when there are no enabled
    /// targets at all (including zero configured targets).
    pub fn uplink_overall_health(&self) -> OverallUplinkHealth {
        overall_uplink_health_of(&self.uplink_snapshot())
    }

    fn sum_uplink_u64<F: Fn(&Arc<UplinkTarget>) -> u64>(&self, f: F) -> u64 {
        self.uplink_targets
            .read()
            .expect("uplink_targets lock poisoned")
            .iter()
            .map(f)
            .sum()
    }

    pub fn uplink_sent_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.sent.load(Ordering::Relaxed))
    }

    pub fn uplink_suppressed_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.suppressed.load(Ordering::Relaxed))
    }

    pub fn uplink_lagged_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.lagged.load(Ordering::Relaxed))
    }

    pub fn uplink_write_failed_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.write_failed.load(Ordering::Relaxed))
    }

    pub fn uplink_disconnected_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.disconnected.load(Ordering::Relaxed))
    }

    pub fn uplink_reconnects_total(&self) -> u64 {
        self.sum_uplink_u64(|t| t.reconnects.load(Ordering::Relaxed))
    }

    /// Count of currently-connected uplink targets, not a single 0/1 flag
    /// -- MAN-42 can spawn multiple independent `uplink::serve` tasks,
    /// each owning its own registered `UplinkTarget` (MAN-44), so this is
    /// simply a count over the registry rather than a separately
    /// maintained value. For the common single-target case this is still
    /// exactly 0 or 1, same as before MAN-32/MAN-42.
    pub fn uplink_connected_count(&self) -> i64 {
        self.uplink_targets
            .read()
            .expect("uplink_targets lock poisoned")
            .iter()
            .filter(|t| t.connected.load(Ordering::Relaxed))
            .count() as i64
    }

    pub fn uplink_connected(&self) -> bool {
        self.uplink_connected_count() > 0
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
            "# HELP manta_spots_dropped_lagged_total Spots dropped because a slow client fell behind and was disconnected.\n",
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
            "# HELP manta_spots_dropped_write_failed_total Spots dropped because a client's socket write timed out or failed before a lag/close error was observed.\n",
        );
        out.push_str("# TYPE manta_spots_dropped_write_failed_total counter\n");
        out.push_str(&format!(
            "manta_spots_dropped_write_failed_total {}\n",
            self.spots_dropped_write_failed_total
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
            "# HELP manta_source_health Per-input-source health (1 = healthy, 0 = unhealthy).\n",
        );
        out.push_str("# TYPE manta_source_health gauge\n");
        for (source, healthy) in self
            .source_health
            .read()
            .expect("source_health lock poisoned")
            .iter()
        {
            out.push_str(&format!(
                "manta_source_health{{source=\"{}\"}} {}\n",
                escape_label_value(source),
                if *healthy { 1 } else { 0 }
            ));
        }

        out.push_str("# HELP manta_uplink_sent_total Spots forwarded to the RBN uplink target.\n");
        out.push_str("# TYPE manta_uplink_sent_total counter\n");
        out.push_str(&format!(
            "manta_uplink_sent_total {}\n",
            self.uplink_sent_total()
        ));

        out.push_str(
            "# HELP manta_uplink_suppressed_total Spots suppressed by dry-run instead of sent to the RBN uplink target.\n",
        );
        out.push_str("# TYPE manta_uplink_suppressed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_suppressed_total {}\n",
            self.uplink_suppressed_total()
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

        // MAN-44: per-target uplink series, additive to the aggregate
        // series above (unchanged in name/type/value) -- new metric
        // NAMES, not labels bolted onto the existing ones, so an existing
        // scrape config/dashboard built against the pre-MAN-44 series
        // shape keeps working untouched.
        let targets = self.uplink_snapshot();

        out.push_str(
            "# HELP manta_uplink_target_connected Whether this RBN uplink target is currently connected (1/0).\n",
        );
        out.push_str("# TYPE manta_uplink_target_connected gauge\n");
        for t in &targets {
            out.push_str(&format!(
                "manta_uplink_target_connected{{target=\"{}\"}} {}\n",
                escape_label_value(&t.label),
                if t.connected { 1 } else { 0 }
            ));
        }

        out.push_str(
            "# HELP manta_uplink_target_sent_total Spots forwarded to this RBN uplink target.\n",
        );
        out.push_str("# TYPE manta_uplink_target_sent_total counter\n");
        for t in &targets {
            out.push_str(&format!(
                "manta_uplink_target_sent_total{{target=\"{}\"}} {}\n",
                escape_label_value(&t.label),
                t.sent
            ));
        }

        out.push_str(
            "# HELP manta_uplink_target_suppressed_total Spots suppressed by dry-run instead of sent to this RBN uplink target.\n",
        );
        out.push_str("# TYPE manta_uplink_target_suppressed_total counter\n");
        for t in &targets {
            out.push_str(&format!(
                "manta_uplink_target_suppressed_total{{target=\"{}\"}} {}\n",
                escape_label_value(&t.label),
                t.suppressed
            ));
        }

        out.push_str(
            "# HELP manta_uplink_target_reconnects_total Times this RBN uplink target's connection was reestablished after dropping.\n",
        );
        out.push_str("# TYPE manta_uplink_target_reconnects_total counter\n");
        for t in &targets {
            out.push_str(&format!(
                "manta_uplink_target_reconnects_total{{target=\"{}\"}} {}\n",
                escape_label_value(&t.label),
                t.reconnects
            ));
        }

        out.push_str(&format!(
            "# HELP manta_uplink_target_recent_reconnects Reconnects for this RBN uplink target within the last {}s (MAN-44 flapping window).\n",
            RECONNECT_WINDOW.as_secs()
        ));
        out.push_str("# TYPE manta_uplink_target_recent_reconnects gauge\n");
        for t in &targets {
            out.push_str(&format!(
                "manta_uplink_target_recent_reconnects{{target=\"{}\"}} {}\n",
                escape_label_value(&t.label),
                t.recent_reconnects
            ));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(label: &str) -> UplinkTargetSpec {
        let (host, port) = label.rsplit_once(':').unwrap();
        UplinkTargetSpec {
            label: label.to_string(),
            host: host.to_string(),
            port: port.parse().unwrap(),
            enabled: true,
            dry_run: false,
        }
    }

    fn disabled_spec(label: &str) -> UplinkTargetSpec {
        UplinkTargetSpec {
            enabled: false,
            ..spec(label)
        }
    }

    #[test]
    fn renders_spot_count_as_a_prometheus_counter() {
        let m = Metrics::new();
        m.record_spot();
        m.record_spot();
        let text = m.render_prometheus_text();
        assert!(text.contains("# TYPE manta_spots_total counter"));
        assert!(text.contains("manta_spots_total 2"));
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

    #[test]
    fn renders_per_source_health_as_labeled_gauge() {
        let m = Metrics::new();
        m.set_source_health("soapy0", true);
        m.set_source_health("kiwi-remote", false);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_source_health{source="kiwi-remote"} 0"#));
        assert!(text.contains(r#"manta_source_health{source="soapy0"} 1"#));
    }

    #[test]
    fn source_health_label_is_escaped_in_prometheus_output() {
        let m = Metrics::new();
        m.set_source_health("weird\"source", true);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_source_health{source="weird\"source"} 1"#));
    }

    // MAN-44: per-target uplink registry.

    #[test]
    fn registered_target_records_its_own_counters_and_aggregates_sum_them() {
        let m = Metrics::new();
        let a = m.register_uplink_target(spec("a.example:7000"));
        let b = m.register_uplink_target(spec("b.example:7000"));

        a.record_sent();
        a.record_sent();
        b.record_sent();
        b.record_suppressed();

        assert_eq!(a.snapshot().sent, 2);
        assert_eq!(b.snapshot().sent, 1);
        assert_eq!(b.snapshot().suppressed, 1);
        // Aggregates are derived, never separately maintained.
        assert_eq!(m.uplink_sent_total(), 3);
        assert_eq!(m.uplink_suppressed_total(), 1);
    }

    #[test]
    fn connected_count_is_per_target_so_one_targets_failure_cannot_clear_another() {
        // MAN-42's round-1 last-writer-wins regression, now enforced
        // structurally: each target owns its own AtomicBool, so
        // unbalanced calls cannot desync a shared counter at all.
        let m = Metrics::new();
        let a = m.register_uplink_target(spec("a.example:7000"));
        let b = m.register_uplink_target(spec("b.example:7000"));

        a.mark_connected();
        assert!(m.uplink_connected());
        b.record_reconnect();
        b.record_reconnect();
        b.record_reconnect();
        assert!(
            m.uplink_connected(),
            "b's failures must not clear a's connection"
        );
        assert_eq!(m.uplink_connected_count(), 1);

        a.mark_disconnected();
        assert!(!m.uplink_connected());
    }

    #[test]
    fn reconnects_outside_the_window_do_not_count_as_recent() {
        let m = Metrics::new();
        let t = m.register_uplink_target(spec("a.example:7000"));
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
        let t = m.register_uplink_target(spec("a.example:7000"));
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
        let a = m.register_uplink_target(spec("a.example:7000"));
        let b = m.register_uplink_target(spec("b.example:7000"));
        let _disabled = m.register_uplink_target(disabled_spec("c.example:7000"));
        a.mark_connected();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Degraded);
        b.mark_connected();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Ok);
    }

    #[test]
    fn overall_uplink_health_is_disabled_when_no_targets_are_enabled() {
        let m = Metrics::new();
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Disabled);
        let _disabled = m.register_uplink_target(disabled_spec("a.example:7000"));
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Disabled);
    }

    #[test]
    fn overall_uplink_health_is_down_when_every_enabled_target_is_down() {
        let m = Metrics::new();
        let _a = m.register_uplink_target(spec("a.example:7000"));
        assert_eq!(m.uplink_overall_health(), OverallUplinkHealth::Down);
    }

    #[test]
    fn a_registered_but_never_connected_target_still_appears_in_snapshots() {
        // Scenario 2 depends on this: a target that has NEVER succeeded
        // must be visible, not absent.
        let m = Metrics::new();
        let _ = m.register_uplink_target(spec("never.example:7000"));
        let snap = m.uplink_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].label, "never.example:7000");
        assert_eq!(snap[0].health, UplinkHealth::Down);
    }

    #[test]
    fn renders_uplink_counters_as_prometheus_metrics() {
        let m = Metrics::new();
        let t = m.register_uplink_target(spec("a.example:7000"));
        t.record_sent();
        t.record_sent();
        t.record_suppressed();
        t.record_reconnect();
        t.record_write_failed(1);
        t.record_disconnected(2);
        t.mark_connected();
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_uplink_sent_total 2"));
        assert!(text.contains("manta_uplink_suppressed_total 1"));
        assert!(text.contains("manta_uplink_reconnects_total 1"));
        assert!(text.contains("manta_uplink_dropped_write_failed_total 1"));
        assert!(text.contains("manta_uplink_dropped_disconnected_total 2"));
        assert!(text.contains("manta_uplink_connected 1"));
    }

    #[test]
    fn renders_per_target_uplink_series_without_changing_the_aggregate_series() {
        let m = Metrics::new();
        let a = m.register_uplink_target(spec("a.example:7000"));
        a.mark_connected();
        a.record_sent();
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_uplink_target_connected{target="a.example:7000"} 1"#));
        assert!(text.contains(r#"manta_uplink_target_sent_total{target="a.example:7000"} 1"#));
        // Existing series keep their exact pre-MAN-44 names and values.
        assert!(text.contains("manta_uplink_sent_total 1"));
        assert!(text.contains("manta_uplink_connected 1"));
    }

    #[test]
    fn target_labels_are_escaped_in_prometheus_output() {
        // Labels come from operator config; a quote or backslash in a
        // hostname must not be able to produce a malformed exposition
        // line.
        let m = Metrics::new();
        let weird = UplinkTargetSpec {
            label: "weird\"host:7000".to_string(),
            host: "weird\"host".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        };
        m.register_uplink_target(weird);
        let text = m.render_prometheus_text();
        assert!(text.contains(r#"manta_uplink_target_connected{target="weird\"host:7000"} 0"#));
    }

    #[test]
    fn uptime_grows_and_never_goes_backwards() {
        let m = Metrics::new();
        let first = m.uptime();
        std::thread::sleep(Duration::from_millis(5));
        let second = m.uptime();
        assert!(second >= first);
    }
}
