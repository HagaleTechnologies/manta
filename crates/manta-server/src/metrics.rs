//! Prometheus text-format metrics. ARCHITECTURE §8: "manta --status hits a
//! local control socket... Prometheus text endpoint... spot rate, active
//! tracks..."; MAN-12 scenario 3 ("operators can inspect health without
//! reading source"). `Metrics` owns what `manta-server` genuinely knows
//! (spots published, connected clients per protocol) and exposes
//! `set_active_tracks`/`set_source_health`/`set_input_health` for the
//! daemon wiring layer to inject engine-owned numbers manta-server has no
//! way to compute itself.

use crate::health::{DecodeWatch, HealthCheck, HealthReport};
use manta_spot::SpotType;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime};

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

fn spot_type_metric_label(t: SpotType) -> &'static str {
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
    lagged: AtomicU64,
    write_failed: AtomicU64,
    disconnected: AtomicU64,
    reconnects: AtomicU64,
}

impl UplinkTarget {
    fn new(label: String, enabled: bool) -> Self {
        Self {
            label,
            enabled,
            connected: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            write_failed: AtomicU64::new(0),
            disconnected: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
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

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
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
    source_health: RwLock<BTreeMap<String, bool>>,
    /// MAN-56: HPSDR's (and any future source's) packet loss/malformed
    /// counters. Engine/input-owned figures, injected by the daemon wiring
    /// layer on a timer -- see `manta-cli`'s `INPUT_HEALTH_POLL_INTERVAL`.
    /// Sources with no wire-packet loss model never call `set_input_health`,
    /// so they publish no series rather than a permanently-zero one.
    input_health: RwLock<BTreeMap<String, InputHealth>>,
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
    /// MAN-128: which listeners (telnet/json/metrics) are currently up, per
    /// `/healthz`'s "listeners are up" check and the `manta_listener_up`
    /// gauge.
    listener_up: RwLock<BTreeMap<String, bool>>,
    /// MAN-128: the decode loop's own liveness, as last observed by the
    /// daemon wiring layer -- see `crate::health::DecodeWatch`.
    decode_watch: Mutex<DecodeWatch>,
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

    pub fn set_source_health(&self, source: &str, healthy: bool) {
        self.source_health
            .write()
            .expect("source_health lock poisoned")
            .insert(source.to_string(), healthy);
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
            for (name, healthy) in sources.iter() {
                checks.push(HealthCheck {
                    name: format!("source:{name}"),
                    ok: *healthy,
                    detail: format!(
                        "source {name}: {}",
                        if *healthy { "healthy" } else { "unhealthy" }
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

        out.push_str(
            "# HELP manta_spots_dropped_shutdown_total Spots abandoned because the daemon shut down while a client was still in its pre-login/handshake phase, before any socket write timed out or failed.\n",
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
                "manta_source_health{{source=\"{source}\"}} {}\n",
                if *healthy { 1 } else { 0 }
            ));
        }

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

        // MAN-128: per-target uplink families -- one read guard for all
        // eight, so a concurrent `register_uplink_target` can't interleave
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

        out.push_str("# HELP manta_uplink_target_suppressed_total Spots suppressed by dry-run instead of sent to this RBN uplink target.\n");
        out.push_str("# TYPE manta_uplink_target_suppressed_total counter\n");
        for t in targets.iter() {
            out.push_str(&format!(
                "manta_uplink_target_suppressed_total{{target=\"{}\"}} {}\n",
                escape_label_value(t.label()),
                t.suppressed_total()
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
}
