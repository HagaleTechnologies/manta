//! Operator-facing status document (MAN-44). ARCHITECTURE §8's "manta
//! status ... live stats" is served as JSON on the metrics listener's
//! `GET /status` (`metrics_http::route`) and rendered for humans by
//! `manta status` (`manta-cli`).
//!
//! Deliberately NOT a `dispensa` ecosystem contract (CLAUDE.md assigns
//! that to the spot schema): this is single-daemon operator tooling, not
//! an ingest contract shared across the ecosystem. `schema_version` is
//! here so a future field removal is detectable by an older CLI talking
//! to a newer daemon. The CLI parses this exact struct (via
//! `manta-server` as a library dependency), so there is no second,
//! independently-drifting definition of the wire shape.

use crate::metrics::{
    overall_uplink_health_of, Metrics, OverallUplinkHealth, UplinkHealth, UplinkTargetSnapshot,
    FLAPPING_RECONNECTS, RECONNECT_WINDOW,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusDoc {
    pub schema_version: u32,
    pub version: String,
    pub uptime_seconds: u64,
    pub spots_total: u64,
    pub telnet_clients: i64,
    pub json_clients: i64,
    pub ws_clients: i64,
    /// The daemon's live decoder-track count, from
    /// `Metrics::active_tracks()`. ARCHITECTURE.md §8's "served but never
    /// populated" caution about `manta_active_tracks` was CORRECTED on
    /// 2026-09-04 (MAN-45): `manta_engine::listen_with_observers`
    /// publishes `TrackManager::active_track_count()` into a shared
    /// handle, and `manta-cli` polls it into `Metrics` every
    /// `ACTIVE_TRACKS_POLL_INTERVAL` (250 ms). That poller is spawned in
    /// the same `--config` branch that starts the listener serving this
    /// document, so any reachable `/status` has a live value behind it --
    /// hardcoding `None` here (MAN-44 review) only made `/status` and
    /// `manta status` disagree with `/metrics` on the same daemon.
    /// Still an `Option` on the wire: `null` is what a daemon built
    /// before that poller existed emits, and `render_human` keeps
    /// rendering that as "n/a" rather than a live-looking `0`.
    pub active_tracks: Option<u64>,
    pub uplink: UplinkStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UplinkStatus {
    pub health: OverallUplinkHealth,
    pub connected_targets: usize,
    pub enabled_targets: usize,
    pub sent_total: u64,
    pub suppressed_total: u64,
    pub reconnects_total: u64,
    pub reconnect_window_seconds: u64,
    pub flapping_threshold: u32,
    /// Never includes `login_callsign` (MAN-44 decision 9) -- knowing
    /// *which* target is broken is the whole point of this document; the
    /// login callsign adds nothing to that question. `UplinkTargetSnapshot`
    /// (from `metrics.rs`) has no such field to begin with, so there is
    /// nothing here that could leak it even by future accident.
    pub targets: Vec<UplinkTargetSnapshot>,
}

impl StatusDoc {
    pub fn from_metrics(metrics: &Metrics) -> Self {
        // One `now`, one registry walk: every uplink field below --
        // `targets`, `connected_targets`, `health`, and the three
        // aggregate totals -- is derived from this SAME snapshot, so none
        // of them can disagree with each other or with the rows in
        // `targets` (MAN-44 code review CR-1, CR-2, CR-3). Before this,
        // `connected_targets` counted the raw `connected` bool while
        // `health` counted `health == Connected` from a second, later
        // snapshot (CR-1); and the three `_total` fields each called a
        // `Metrics` getter that took its own fresh registry read at its
        // own later instant (CR-2), so `/status` could report totals that
        // disagreed with `sum(targets[].sent/suppressed/reconnects)` for a
        // spot forwarded in between.
        let targets = metrics.uplink_snapshot_at(Instant::now());
        let enabled_targets = targets.iter().filter(|t| t.enabled).count();
        let connected_targets = targets
            .iter()
            .filter(|t| t.health == UplinkHealth::Connected)
            .count();
        let sent_total = targets.iter().map(|t| t.sent).sum();
        let suppressed_total = targets.iter().map(|t| t.suppressed).sum();
        let reconnects_total = targets.iter().map(|t| t.reconnects).sum();
        let health = overall_uplink_health_of(&targets);
        StatusDoc {
            schema_version: 1,
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: metrics.uptime().as_secs(),
            spots_total: metrics.spots_total(),
            telnet_clients: metrics.telnet_clients(),
            json_clients: metrics.json_clients(),
            ws_clients: metrics.ws_clients(),
            active_tracks: Some(metrics.active_tracks()),
            uplink: UplinkStatus {
                health,
                connected_targets,
                enabled_targets,
                sent_total,
                suppressed_total,
                reconnects_total,
                reconnect_window_seconds: RECONNECT_WINDOW.as_secs(),
                flapping_threshold: FLAPPING_RECONNECTS,
                targets,
            },
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("StatusDoc is infallibly serializable")
    }
}

fn overall_health_label(health: OverallUplinkHealth) -> &'static str {
    match health {
        OverallUplinkHealth::Ok => "OK",
        OverallUplinkHealth::Degraded => "DEGRADED",
        OverallUplinkHealth::Down => "DOWN",
        OverallUplinkHealth::Disabled => "DISABLED",
    }
}

fn target_health_label(health: UplinkHealth) -> &'static str {
    match health {
        UplinkHealth::Disabled => "disabled",
        UplinkHealth::Connected => "connected",
        UplinkHealth::Flapping => "flapping",
        UplinkHealth::Down => "down",
    }
}

/// One screen an operator can read without a manual (MAN-44). Fixed-width
/// columns, kept inside 80 (the target table renders 78 for the plan's own
/// worked example -- `telnet.reversebeacon.net:7000`, the longest
/// realistic RBN hostname, plus a 5-digit sent count); the uplink verdict
/// comes first because that is the question `manta status` exists to
/// answer.
pub fn render_human(doc: &StatusDoc) -> String {
    let mut out = String::new();
    let hours = doc.uptime_seconds / 3600;
    let minutes = (doc.uptime_seconds % 3600) / 60;
    out.push_str(&format!("manta status — daemon up {hours}h {minutes}m\n\n"));
    out.push_str(&format!("  spots published      {}\n", doc.spots_total));
    out.push_str(&format!(
        "  telnet clients       {}      json/ws clients  {}\n",
        doc.telnet_clients,
        doc.json_clients + doc.ws_clients
    ));
    // A current daemon always sends a real count (see the field's doc
    // comment); `None` means a pre-MAN-45 daemon that never populated the
    // gauge, and that is labeled rather than shown as a
    // misleadingly-live-looking 0.
    out.push_str(&format!(
        "  active tracks  {}\n\n",
        match doc.active_tracks {
            Some(n) => n.to_string(),
            None => "n/a".to_string(),
        }
    ));

    if doc.uplink.targets.is_empty() {
        out.push_str("RBN uplink: not configured\n");
        return out;
    }

    out.push_str(&format!(
        "RBN uplink: {} — {} of {} enabled targets connected\n\n",
        overall_health_label(doc.uplink.health),
        doc.uplink.connected_targets,
        doc.uplink.enabled_targets
    ));
    out.push_str(&format!(
        "  {:<30} {:<9} {:>6} {:>8} {:>8} {:>10}\n",
        "TARGET", "STATE", "SENT", "SUPPR", "RECONN", "RECENT(5m)"
    ));
    for t in &doc.uplink.targets {
        out.push_str(&format!(
            "  {:<30} {:<9} {:>6} {:>8} {:>8} {:>10}\n",
            t.label,
            target_health_label(t.health),
            t.sent,
            t.suppressed,
            t.reconnects,
            t.recent_reconnects
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::UplinkTargetSpec;

    fn metrics_fixture() -> Metrics {
        let m = Metrics::new();
        let a = m.register_uplink_target(UplinkTargetSpec {
            label: "a.example:7000".to_string(),
            host: "a.example".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        });
        a.mark_connected();
        a.record_sent();

        let b = m.register_uplink_target(UplinkTargetSpec {
            label: "b.example:7000".to_string(),
            host: "b.example".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        });
        b.record_reconnect();
        b.record_reconnect();
        b.record_reconnect();

        m
    }

    #[test]
    fn status_doc_serializes_a_stable_versioned_shape() {
        let doc = StatusDoc::from_metrics(&metrics_fixture());
        let v: serde_json::Value = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert!(v["uptime_seconds"].is_u64());
        assert_eq!(v["uplink"]["health"], "degraded");
        assert_eq!(v["uplink"]["targets"][0]["target"], "a.example:7000");
        assert_eq!(v["uplink"]["targets"][0]["health"], "connected");
        // Decision 9: the login callsign is never exposed.
        assert!(!serde_json::to_string(&doc)
            .unwrap()
            .contains("login_callsign"));
    }

    #[test]
    fn status_doc_round_trips_so_the_cli_parses_exactly_what_the_daemon_emits() {
        let doc = StatusDoc::from_metrics(&metrics_fixture());
        let back: StatusDoc = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn human_render_shows_each_target_state_sent_suppressed_and_recent_reconnects() {
        let out = render_human(&StatusDoc::from_metrics(&metrics_fixture()));
        assert!(out.contains("RBN uplink: DEGRADED"));
        assert!(out.contains("a.example:7000"));
        assert!(out.contains("connected"));
        assert!(out.contains("flapping"));
    }

    #[test]
    fn status_reports_the_same_live_active_track_count_metrics_does() {
        // MAN-44 review: `from_metrics` hardcoded `None`, so `/status`
        // and `manta status` reported "n/a" on a daemon whose
        // `/metrics` was reporting a real count from MAN-45's poller.
        let m = metrics_fixture();
        m.set_active_tracks(7);
        let doc = StatusDoc::from_metrics(&m);
        assert_eq!(doc.active_tracks, Some(7));
        assert!(render_human(&doc).contains("active tracks  7"));
    }

    #[test]
    fn human_render_marks_an_absent_active_track_count_so_it_is_not_read_as_live() {
        // Only a daemon older than MAN-45's poller sends `null` here.
        // Surfacing that as `0` in a health view would read as a live
        // "no tracks", which is exactly what it is not.
        let mut doc = StatusDoc::from_metrics(&metrics_fixture());
        doc.active_tracks = None;
        assert!(render_human(&doc).contains("active tracks  n/a"));
    }

    #[test]
    fn human_render_of_a_daemon_with_no_uplink_configured_says_so_plainly() {
        let doc = StatusDoc::from_metrics(&Metrics::new());
        let out = render_human(&doc);
        assert!(out.contains("RBN uplink: not configured"));
    }

    #[test]
    fn status_doc_from_a_healthy_single_target_daemon_reports_ok() {
        let m = Metrics::new();
        let a = m.register_uplink_target(UplinkTargetSpec {
            label: "a.example:7000".to_string(),
            host: "a.example".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        });
        a.mark_connected();
        let doc = StatusDoc::from_metrics(&m);
        assert_eq!(doc.uplink.health, OverallUplinkHealth::Ok);
        assert_eq!(doc.uplink.connected_targets, 1);
        assert_eq!(doc.uplink.enabled_targets, 1);
        let out = render_human(&doc);
        assert!(out.contains("RBN uplink: OK"));
    }

    #[test]
    fn a_target_that_is_flapping_and_momentarily_connected_does_not_count_as_connected() {
        // CR-1 regression: `connected_targets` must come from the same
        // classification as `health` (health == Connected), not the raw
        // `connected` bool -- otherwise a target that is flapping AND
        // currently connected made `from_metrics` say both "DOWN" and "1
        // of 1 enabled targets connected" at once. That is exactly
        // scenario 2's steady state (plan decision 5): a target
        // reconnecting every 60s is momentarily connected whenever you
        // happen to look.
        let m = Metrics::new();
        let a = m.register_uplink_target(UplinkTargetSpec {
            label: "a.example:7000".to_string(),
            host: "a.example".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        });
        a.mark_connected();
        let t0 = std::time::Instant::now();
        for _ in 0..FLAPPING_RECONNECTS {
            a.record_reconnect_at(t0);
        }

        let doc = StatusDoc::from_metrics(&m);
        assert_eq!(doc.uplink.targets[0].health, UplinkHealth::Flapping);
        assert_eq!(doc.uplink.health, OverallUplinkHealth::Down);
        assert_eq!(
            doc.uplink.connected_targets, 0,
            "a flapping target must not count toward connected_targets even though its raw `connected` bit is set"
        );

        let out = render_human(&doc);
        assert!(
            out.contains("RBN uplink: DOWN — 0 of 1 enabled targets connected"),
            "summary must not name a connected count that contradicts the DOWN verdict: {out}"
        );
    }

    #[test]
    fn rendered_target_table_fits_in_eighty_columns_for_the_longest_documented_hostname() {
        // D3: the plan's manual verification step is "confirm the summary
        // fits in 80 columns with a long RBN hostname" -- exercised here
        // with the real hostname the runbook and ADR both use as that
        // "long" example, plus a 5-digit sent count matching the
        // runbook's own sample.
        let m = Metrics::new();
        let a = m.register_uplink_target(UplinkTargetSpec {
            label: "telnet.reversebeacon.net:7000".to_string(),
            host: "telnet.reversebeacon.net".to_string(),
            port: 7000,
            enabled: true,
            dry_run: false,
        });
        a.mark_connected();
        for _ in 0..18001 {
            a.record_sent();
        }
        let doc = StatusDoc::from_metrics(&m);
        let out = render_human(&doc);
        for line in out.lines() {
            let width = line.chars().count();
            assert!(
                width <= 80,
                "line exceeds 80 columns ({width} chars): {line:?}"
            );
        }
    }
}
