//! Prometheus text-format metrics. ARCHITECTURE §8: "manta --status hits a
//! local control socket... Prometheus text endpoint... spot rate, active
//! tracks..."; MAN-12 scenario 3 ("operators can inspect health without
//! reading source"). `Metrics` owns what `manta-server` genuinely knows
//! (spots published, connected clients per protocol) and exposes
//! `set_active_tracks`/`set_source_health` for the daemon wiring layer to
//! inject engine-owned numbers manta-server has no way to compute itself.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::RwLock;

#[derive(Default)]
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
    uplink_sent_total: AtomicU64,
    uplink_suppressed_total: AtomicU64,
    uplink_lagged_total: AtomicU64,
    uplink_write_failed_total: AtomicU64,
    uplink_disconnected_total: AtomicU64,
    uplink_reconnects_total: AtomicU64,
    /// Count of currently-connected uplink targets, not a single 0/1 flag
    /// -- MAN-42 can spawn multiple independent `uplink::serve` tasks
    /// sharing this one `Metrics`, and each only increments/decrements its
    /// own connect/disconnect transition (see `mark_uplink_connected`/
    /// `mark_uplink_disconnected`). A shared last-writer-wins boolean would
    /// let one target's failed reconnect attempt clear the gauge while
    /// another target is genuinely, unrelatedly connected (round-1 review
    /// finding on MAN-42/PR). For the common single-target case this is
    /// still exactly 0 or 1, same as before MAN-42.
    uplink_connected_count: AtomicI64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
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

    // MAN-32: RBN uplink counters. ARCHITECTURE §8's "every
    // dropped/evicted/suppressed item is counted" invariant applies here
    // too -- a dry-run-suppressed or lag-dropped spot must be visible,
    // not silent.

    pub fn record_uplink_sent(&self) {
        self.uplink_sent_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn uplink_sent_total(&self) -> u64 {
        self.uplink_sent_total.load(Ordering::Relaxed)
    }

    pub fn record_uplink_suppressed(&self) {
        self.uplink_suppressed_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn uplink_suppressed_total(&self) -> u64 {
        self.uplink_suppressed_total.load(Ordering::Relaxed)
    }

    pub fn record_uplink_lagged(&self, n: u64) {
        self.uplink_lagged_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn uplink_lagged_total(&self) -> u64 {
        self.uplink_lagged_total.load(Ordering::Relaxed)
    }

    /// A write to the uplink target's socket itself timed out or failed
    /// (PR #80 review, round 3, tightened round 4, narrowed round 8):
    /// distinct from `uplink_lagged_total` (broadcast-channel lag, not a
    /// network-level write failure) AND from `uplink_disconnected_total`
    /// (every OTHER cause that abandons the connection's queued backlog
    /// without a write itself having failed -- a rate-limit disconnect, a
    /// protocol violation, a stalled login prompt, a shutdown cancelling
    /// an in-flight write). Round 8 caught this counter being used for
    /// those non-write causes too, which contradicts its own name and
    /// Prometheus HELP text ("write timed out or failed") and would
    /// misdirect operational alerts/diagnosis toward the wrong failure
    /// mode. `n` covers the spot whose write just failed plus whatever
    /// was still retained in the receiver's own buffer and is now
    /// abandoned along with it -- matching `record_write_failed`'s
    /// identical `1 + rx.len()` convention on the inbound telnet/JSON
    /// write path (round-11 review finding there).
    pub fn record_uplink_write_failed(&self, n: u64) {
        self.uplink_write_failed_total
            .fetch_add(n, Ordering::Relaxed);
    }

    /// The uplink connection is being torn down (and its queued backlog
    /// abandoned) for a reason OTHER than a failed/timed-out write itself
    /// (PR #80 review, round 8): a rate-limit disconnect, a protocol
    /// violation (oversized/invalid response line, target closed the
    /// connection), a stalled login prompt, or a clean shutdown
    /// cancelling an in-flight write. Kept separate from
    /// `uplink_write_failed_total` specifically so that counter's own
    /// name and HELP text stay accurate for alerting/diagnosis -- see its
    /// doc comment. `n` is the same "current item (if any) + remaining
    /// backlog" shape as `record_uplink_write_failed`.
    pub fn record_uplink_disconnected(&self, n: u64) {
        self.uplink_disconnected_total
            .fetch_add(n, Ordering::Relaxed);
    }

    pub fn uplink_disconnected_total(&self) -> u64 {
        self.uplink_disconnected_total.load(Ordering::Relaxed)
    }

    pub fn uplink_write_failed_total(&self) -> u64 {
        self.uplink_write_failed_total.load(Ordering::Relaxed)
    }

    pub fn record_uplink_reconnect(&self) {
        self.uplink_reconnects_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn uplink_reconnects_total(&self) -> u64 {
        self.uplink_reconnects_total.load(Ordering::Relaxed)
    }

    /// Call exactly once per `uplink::serve` task each time IT transitions
    /// from disconnected to connected -- pair with exactly one
    /// `mark_uplink_disconnected` when that same task's connection ends.
    /// Unbalanced calls would desync the shared count across tasks.
    pub fn mark_uplink_connected(&self) {
        self.uplink_connected_count.fetch_add(1, Ordering::Relaxed);
    }

    /// See `mark_uplink_connected` -- call only to undo a prior
    /// `mark_uplink_connected` from the same task's same connection.
    pub fn mark_uplink_disconnected(&self) {
        self.uplink_connected_count.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn uplink_connected(&self) -> bool {
        self.uplink_connected_count.load(Ordering::Relaxed) > 0
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
                "manta_source_health{{source=\"{source}\"}} {}\n",
                if *healthy { 1 } else { 0 }
            ));
        }

        out.push_str("# HELP manta_uplink_sent_total Spots forwarded to the RBN uplink target.\n");
        out.push_str("# TYPE manta_uplink_sent_total counter\n");
        out.push_str(&format!(
            "manta_uplink_sent_total {}\n",
            self.uplink_sent_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_uplink_suppressed_total Spots suppressed by dry-run instead of sent to the RBN uplink target.\n",
        );
        out.push_str("# TYPE manta_uplink_suppressed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_suppressed_total {}\n",
            self.uplink_suppressed_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_lagged_total Spots the uplink fell behind on and lost before its next reconnect.\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_lagged_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_lagged_total {}\n",
            self.uplink_lagged_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_write_failed_total Spots dropped because a write to the RBN uplink target's socket timed out or failed.\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_write_failed_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_write_failed_total {}\n",
            self.uplink_write_failed_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_uplink_dropped_disconnected_total Spots dropped when the RBN uplink connection was torn down for a reason other than a failed write (rate limit, protocol violation, stalled login, shutdown).\n",
        );
        out.push_str("# TYPE manta_uplink_dropped_disconnected_total counter\n");
        out.push_str(&format!(
            "manta_uplink_dropped_disconnected_total {}\n",
            self.uplink_disconnected_total.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP manta_uplink_reconnects_total Times the RBN uplink connection was reestablished after dropping.\n");
        out.push_str("# TYPE manta_uplink_reconnects_total counter\n");
        out.push_str(&format!(
            "manta_uplink_reconnects_total {}\n",
            self.uplink_reconnects_total.load(Ordering::Relaxed)
        ));

        out.push_str(
            "# HELP manta_uplink_connected Count of configured RBN uplink targets currently connected.\n",
        );
        out.push_str("# TYPE manta_uplink_connected gauge\n");
        out.push_str(&format!(
            "manta_uplink_connected {}\n",
            self.uplink_connected_count.load(Ordering::Relaxed)
        ));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // MAN-32: RBN uplink counters.

    #[test]
    fn uplink_counters_start_at_zero_and_increment() {
        let m = Metrics::new();
        assert_eq!(m.uplink_sent_total(), 0);
        m.record_uplink_sent();
        assert_eq!(m.uplink_sent_total(), 1);

        assert_eq!(m.uplink_suppressed_total(), 0);
        m.record_uplink_suppressed();
        assert_eq!(m.uplink_suppressed_total(), 1);

        assert_eq!(m.uplink_lagged_total(), 0);
        m.record_uplink_lagged(3);
        assert_eq!(m.uplink_lagged_total(), 3);

        assert_eq!(m.uplink_write_failed_total(), 0);
        m.record_uplink_write_failed(1);
        assert_eq!(m.uplink_write_failed_total(), 1);
        m.record_uplink_write_failed(4); // e.g. the failed spot + 3 abandoned backlog
        assert_eq!(m.uplink_write_failed_total(), 5);

        assert_eq!(m.uplink_disconnected_total(), 0);
        m.record_uplink_disconnected(2);
        assert_eq!(m.uplink_disconnected_total(), 2);

        assert_eq!(m.uplink_reconnects_total(), 0);
        m.record_uplink_reconnect();
        assert_eq!(m.uplink_reconnects_total(), 1);

        assert!(!m.uplink_connected());
        m.mark_uplink_connected();
        assert!(m.uplink_connected());
        m.mark_uplink_disconnected();
        assert!(!m.uplink_connected());
    }

    #[test]
    fn renders_uplink_counters_as_prometheus_metrics() {
        let m = Metrics::new();
        m.record_uplink_sent();
        m.record_uplink_sent();
        m.record_uplink_suppressed();
        m.record_uplink_reconnect();
        m.record_uplink_write_failed(1);
        m.record_uplink_disconnected(2);
        m.mark_uplink_connected();
        let text = m.render_prometheus_text();
        assert!(text.contains("manta_uplink_sent_total 2"));
        assert!(text.contains("manta_uplink_suppressed_total 1"));
        assert!(text.contains("manta_uplink_reconnects_total 1"));
        assert!(text.contains("manta_uplink_dropped_write_failed_total 1"));
        assert!(text.contains("manta_uplink_dropped_disconnected_total 2"));
        assert!(text.contains("manta_uplink_connected 1"));
    }

    // MAN-42: multiple uplink::serve tasks share one Metrics, so
    // uplink_connected must reflect how many are currently connected, not
    // a single last-writer-wins boolean -- otherwise one target's failed
    // reconnect attempt can flip the gauge to "disconnected" while another
    // target is genuinely, unrelatedly connected (round-1 review finding
    // on MAN-42/PR).
    #[test]
    fn uplink_connected_reflects_multiple_independently_tracked_targets() {
        let m = Metrics::new();
        assert!(!m.uplink_connected());

        // Target A connects.
        m.mark_uplink_connected();
        assert!(m.uplink_connected());

        // Target B repeatedly fails to connect/reconnect. It was never
        // itself marked connected, so its failures must not touch the
        // shared gauge -- A's connection must still read as connected.
        m.record_uplink_reconnect();
        m.record_uplink_reconnect();
        m.record_uplink_reconnect();
        assert!(
            m.uplink_connected(),
            "an unrelated target's failed reconnects must not clear the gauge \
             while another target is genuinely connected"
        );

        // A itself drops -- now nothing is connected.
        m.mark_uplink_disconnected();
        assert!(!m.uplink_connected());
    }
}
