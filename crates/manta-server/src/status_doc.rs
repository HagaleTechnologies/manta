//! Operator-facing status document (MAN-44). Not `status.rs`: that is
//! MAN-122's periodic log status line. ARCHITECTURE §8's "manta
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

#[cfg(test)]
use crate::metrics::SourceLossReason;
use crate::metrics::{
    overall_uplink_health_of, Metrics, OverallUplinkHealth, UplinkHealth, UplinkTargetSnapshot,
    FLAPPING_RECONNECTS, RECONNECT_WINDOW,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// The one `schema_version` this build both emits and understands.
///
/// Public because it is half of a wire contract, not a private detail:
/// `manta-cli`'s `parse_status_doc` rejects any other version outright
/// rather than reading a newer daemon's document with this build's
/// semantics (Codex review, PR #95). A bump here means an INCOMPATIBLE
/// change -- a removed or re-meaning'd field. Purely additive fields do
/// not bump it: `serde` ignores unknown keys, so an older CLI keeps
/// reading a newer daemon correctly, which is the whole reason a version
/// mismatch can be treated as fatal rather than best-effort.
/// Additive fields are still printed by `manta status --json` (MAN-285).
pub const STATUS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusDoc {
    pub schema_version: u32,
    pub version: String,
    pub uptime_seconds: u64,
    pub spots_total: u64,
    pub telnet_clients: i64,
    pub json_clients: i64,
    pub ws_clients: i64,
    /// The live decoder-track count, from the same gauge `/metrics`
    /// publishes as `manta_active_tracks`. `Some` for any daemon that
    /// builds this document: MAN-45 (ARCHITECTURE.md §8, corrected
    /// 2026-09-04) gave `Metrics::set_active_tracks` a real production
    /// call site -- `manta-cli`'s server runtime polls
    /// `manta_engine::listen_with_observers`'s shared handle into
    /// `Metrics` every `ACTIVE_TRACKS_POLL_INTERVAL` (250ms) -- so this
    /// is a live number, not the frozen `0` the earlier "served but never
    /// populated" caution warned about (MAN-44 review CR-B).
    ///
    /// Stays `Option` for the schema contract, not because the daemon
    /// still withholds it: an older daemon (or a future one that drops
    /// the field) serializes `null`, and `render_human` prints "n/a"
    /// rather than a fabricated `0`.
    pub active_tracks: Option<u64>,
    pub uplink: UplinkStatus,
    /// MAN-14: each input's hearing, in config order. Present only with
    /// two or more `[[input]]` entries (additive, so `schema_version`
    /// stays 1 and a one-input document is unchanged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputHearingSnapshot>,
    /// MAN-533: each live source's health and lost-connection record,
    /// sorted by name (the `manta_source_health` label set). Additive, so
    /// `schema_version` stays 1: an older CLI ignores it, and a document
    /// from an older daemon parses with it empty. Omitted while no source
    /// has reported health. `last_loss_reason` is a `String` on the wire,
    /// not the closed `SourceLossReason`, so an older CLI still reads a
    /// newer daemon's label instead of refusing the whole document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceStatus>,
}

/// MAN-533: one live source's row in `GET /status`'s `sources`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    /// The same label as `manta_source_health{source}`.
    pub name: String,
    /// `manta_source_health` as a boolean.
    pub healthy: bool,
    /// Connections the reconnecting source dropped. A failed reopen
    /// attempt is not a loss.
    pub losses_total: u64,
    /// `no usable samples` or `read failed`; `null` before the first loss.
    pub last_loss_reason: Option<String>,
    /// Whole seconds since the last loss; `null` before the first loss.
    pub last_loss_age_seconds: Option<f64>,
}

impl SourceStatus {
    fn rows(metrics: &Metrics, now: Instant) -> Vec<Self> {
        metrics
            .source_snapshot_at(now)
            .into_iter()
            .map(|s| SourceStatus {
                name: s.name,
                healthy: s.healthy,
                losses_total: s.losses_total,
                last_loss_reason: s.last_loss.map(|(reason, _)| reason.label().to_string()),
                last_loss_age_seconds: s.last_loss.map(|(_, age)| age.as_secs_f64().round()),
            })
            .collect()
    }
}

/// MAN-14: one input's row in `GET /status`'s `inputs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputHearingSnapshot {
    pub name: String,
    pub spots_total: u64,
    pub distinct_calls: u64,
    pub cross_input_dupes_total: u64,
    pub silent: bool,
    /// Seconds since this input's latest spot; `null` before its first.
    pub last_spot_age_seconds: Option<f64>,
}

impl InputHearingSnapshot {
    fn rows(metrics: &Metrics, now: Instant) -> Vec<Self> {
        let rows = metrics.input_hearing();
        if rows.len() < 2 {
            return Vec::new();
        }
        rows.into_iter()
            .map(|r| InputHearingSnapshot {
                name: r.name,
                spots_total: r.spots,
                distinct_calls: r.distinct_calls,
                cross_input_dupes_total: r.cross_input_dupes,
                silent: r.silent,
                last_spot_age_seconds: r
                    .last_spot
                    .map(|at| now.saturating_duration_since(at).as_secs_f64().round()),
            })
            .collect()
    }
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
        let now = Instant::now();
        let targets = metrics.uplink_snapshot_at(now);
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
            schema_version: STATUS_SCHEMA_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: metrics.uptime().as_secs(),
            spots_total: metrics.spots_total(),
            telnet_clients: metrics.telnet_clients(),
            json_clients: metrics.json_clients(),
            ws_clients: metrics.ws_clients(),
            // MAN-44 review CR-B: the live gauge, not a hardcoded `None`.
            // `manta-cli`'s poller (`ACTIVE_TRACKS_POLL_INTERVAL`, 250ms)
            // has fed `set_active_tracks` since MAN-45, so discarding it
            // here made `/status` and every `manta status` report
            // `null`/"n/a" while `/metrics` reported the real count.
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
            inputs: InputHearingSnapshot::rows(metrics, now),
            sources: SourceStatus::rows(metrics, now),
        }
    }

    /// Pretty-printed JSON with DEL and the C1 controls (U+007F-U+009F)
    /// `\u`-escaped as well (Codex review, PR #95).
    pub fn to_json(&self) -> String {
        escape_del_and_c1(
            &serde_json::to_string_pretty(self).expect("StatusDoc is infallibly serializable"),
        )
    }
}

/// Pretty-prints the parsed daemon response for `manta status --json`
/// (MAN-285), preserving unknown fields and escaping DEL/C1 controls.
pub fn value_to_json(value: &serde_json::Value) -> String {
    escape_del_and_c1(
        &serde_json::to_string_pretty(value).expect("JSON Value is infallibly serializable"),
    )
}

/// serde_json escapes only U+0000-U+001F, so DEL/C1 controls would
/// otherwise reach the terminal raw through `manta status --json` when a
/// spoofed endpoint puts them in a string. serde_json never emits them
/// outside a string literal, so escaping each one keeps the output valid
/// JSON with the same decoded value.
fn escape_del_and_c1(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if ('\u{7f}'..='\u{9f}').contains(&c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
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

/// Makes a daemon-supplied string safe to write to the operator's terminal
/// (Codex review, PR #95). `manta status --addr` can be pointed at any
/// endpoint, and serde_json decodes `\u001b`, `\n` and friends inside a
/// JSON string, so a spoofed or hostile endpoint could otherwise inject
/// ANSI/OSC sequences or forge extra rows through a target label. Every
/// Unicode control character (`char::is_control`: C0, DEL, C1) is replaced
/// by its `char::escape_default` spelling (`\u{1b}`, `\n`, ...); all other
/// characters, including non-control format characters, pass through
/// unchanged. Only human-facing text goes through this: `StatusDoc` keeps
/// the original value, and `--json` output is escaped by `to_json` instead.
pub fn escape_for_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// One screen an operator can read without a manual (MAN-44). Fixed-width
/// columns, kept inside 80 (the target table renders 78 for the runbook's own
/// worked example -- a 29-character `host:port`, as long as RBN-style
/// hostnames get -- plus a 5-digit sent count); the uplink verdict
/// comes first because that is the question `manta status` exists to
/// answer.
pub fn render_human(doc: &StatusDoc) -> String {
    let mut out = String::new();
    let hours = doc.uptime_seconds / 3600;
    let minutes = (doc.uptime_seconds % 3600) / 60;
    out.push_str(&format!("manta status — daemon up {hours}h {minutes}m\n\n"));
    out.push_str(&format!("  spots published      {}\n", doc.spots_total));
    // Saturating (MAN-281): the counts are peer-supplied through `manta
    // status --addr`, so a spoofed `i64::MAX` must not panic or wrap.
    out.push_str(&format!(
        "  telnet clients       {}      json/ws clients  {}\n",
        doc.telnet_clients,
        doc.json_clients.saturating_add(doc.ws_clients)
    ));
    // A daemon that reports no `active_tracks` at all (an older one, or a
    // future one that drops the field) is shown as "n/a" rather than a
    // fabricated 0 -- a live daemon always sends the real count.
    out.push_str(&format!(
        "  active tracks  {}\n\n",
        match doc.active_tracks {
            Some(n) => n.to_string(),
            None => "n/a".to_string(),
        }
    ));

    // MAN-533: each live source's state and its last lost connection, so a
    // receiver that was dropped for sending nothing usable stands out.
    if !doc.sources.is_empty() {
        out.push_str(&format!(
            "Sources:\n  {:<32} {:<5} {:>6}  {}\n",
            "NAME", "STATE", "LOST", "LAST LOSS"
        ));
        for s in &doc.sources {
            let last_loss = match (s.last_loss_age_seconds, &s.last_loss_reason) {
                (Some(age), Some(reason)) => {
                    format!("{age:.0}s ago, {}", escape_for_terminal(reason))
                }
                (Some(age), None) => format!("{age:.0}s ago"),
                (None, Some(reason)) => escape_for_terminal(reason),
                (None, None) => "never".to_string(),
            };
            out.push_str(&format!(
                "  {:<32} {:<5} {:>6}  {}\n",
                escape_for_terminal(&s.name),
                if s.healthy { "up" } else { "down" },
                s.losses_total,
                last_loss
            ));
        }
        out.push('\n');
    }

    // MAN-14: each input's hearing, so a deaf receiver stands out.
    if !doc.inputs.is_empty() {
        out.push_str(&format!(
            "Inputs:\n  {:<32} {:>8} {:>8} {:>8} {:>10} {:>7}\n",
            "NAME", "SPOTS", "CALLS", "XDUPES", "LAST SPOT", "SILENT"
        ));
        for i in &doc.inputs {
            out.push_str(&format!(
                "  {:<32} {:>8} {:>8} {:>8} {:>10} {:>7}\n",
                escape_for_terminal(&i.name),
                i.spots_total,
                i.distinct_calls,
                i.cross_input_dupes_total,
                match i.last_spot_age_seconds {
                    Some(age) => format!("{age:.0}s ago"),
                    None => "never".to_string(),
                },
                if i.silent { "yes" } else { "no" }
            ));
        }
        out.push('\n');
    }

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
            escape_for_terminal(&t.label),
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

    fn metrics_fixture() -> Metrics {
        let m = Metrics::new();
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
        a.mark_connected();
        a.record_sent();

        let b = m.register_uplink_target("b.example:7000".to_string(), true);
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

    /// MAN-14: two or more inputs add `inputs` rows; one input adds none.
    #[test]
    fn status_doc_carries_input_hearing_rows_only_with_two_or_more_inputs() {
        let m = Metrics::new();
        m.register_input_hearing(&["only".to_string()]);
        let json = serde_json::to_string(&StatusDoc::from_metrics(&m)).unwrap();
        assert!(!json.contains("\"inputs\""), "{json}");
        assert!(json.starts_with("{\"schema_version\":1,"), "{json}");

        let m = Metrics::new();
        m.register_input_hearing(&["live".to_string(), "deaf".to_string()]);
        m.record_input_spot("live");
        m.record_input_distinct_call("live");
        m.set_input_silent("deaf", true);
        let doc = StatusDoc::from_metrics(&m);
        let json = serde_json::to_string(&doc).unwrap();
        assert!(
            json.contains(
                "\"inputs\":[{\"name\":\"live\",\"spots_total\":1,\"distinct_calls\":1,\
                 \"cross_input_dupes_total\":0,\"silent\":false,\"last_spot_age_seconds\":0.0},\
                 {\"name\":\"deaf\",\"spots_total\":0,\"distinct_calls\":0,\
                 \"cross_input_dupes_total\":0,\"silent\":true,\"last_spot_age_seconds\":null}]"
            ),
            "{json}"
        );
        let parsed: StatusDoc = serde_json::from_str(&doc.to_json()).unwrap();
        assert_eq!(parsed, doc);
        let human = render_human(&doc);
        assert!(human.contains("Inputs:"), "{human}");
        let deaf = human
            .lines()
            .find(|l| l.trim_start().starts_with("deaf"))
            .unwrap();
        assert!(deaf.contains("never") && deaf.ends_with("yes"), "{deaf}");
        assert!(human.lines().all(|l| l.chars().count() <= 80), "{human}");
    }

    /// MAN-533: a `kiwi` lost 3 s ago for sending nothing usable and a
    /// healthy `file`, as `Metrics` holds them.
    fn sources_fixture() -> Metrics {
        let m = Metrics::new();
        m.set_source_health("kiwi", true);
        m.set_source_health("file", true);
        let three_s_ago = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3))
            .expect("the monotonic clock is past 3 s");
        m.record_source_loss_at("kiwi", SourceLossReason::NoUsableSamples, three_s_ago);
        m.set_source_health("kiwi", false);
        m
    }

    #[test]
    fn status_doc_carries_one_source_row_per_registered_source() {
        let doc = StatusDoc::from_metrics(&sources_fixture());
        let json = serde_json::to_string(&doc).unwrap();
        assert!(
            json.contains(
                "\"sources\":[{\"name\":\"file\",\"healthy\":true,\"losses_total\":0,\
                 \"last_loss_reason\":null,\"last_loss_age_seconds\":null},\
                 {\"name\":\"kiwi\",\"healthy\":false,\"losses_total\":1,\
                 \"last_loss_reason\":\"no usable samples\",\"last_loss_age_seconds\":3.0}]"
            ),
            "{json}"
        );
        assert!(json.starts_with("{\"schema_version\":1,"), "{json}");
        let parsed: StatusDoc = serde_json::from_str(&doc.to_json()).unwrap();
        assert_eq!(parsed, doc);
    }

    #[test]
    fn status_doc_omits_sources_when_none_is_registered() {
        let json = serde_json::to_string(&StatusDoc::from_metrics(&metrics_fixture())).unwrap();
        assert!(!json.contains("\"sources\""), "{json}");
    }

    #[test]
    fn a_document_without_sources_still_parses() {
        // An older daemon sends no `sources` key at all.
        let mut value = serde_json::to_value(StatusDoc::from_metrics(&sources_fixture())).unwrap();
        assert!(value["sources"].is_array());
        value.as_object_mut().unwrap().remove("sources");
        let parsed: StatusDoc = serde_json::from_value(value).unwrap();
        assert!(parsed.sources.is_empty());
        assert!(!render_human(&parsed).contains("Sources:"));
    }

    #[test]
    fn an_unknown_loss_reason_label_still_parses() {
        // A newer daemon may add a reason this build does not know.
        let json = serde_json::to_string(&StatusDoc::from_metrics(&sources_fixture()))
            .unwrap()
            .replace(
                "\"last_loss_reason\":\"no usable samples\"",
                "\"last_loss_reason\":\"something new\"",
            );
        let parsed: StatusDoc = serde_json::from_str(&json).unwrap();
        let kiwi = parsed.sources.iter().find(|s| s.name == "kiwi").unwrap();
        assert_eq!(kiwi.last_loss_reason.as_deref(), Some("something new"));
    }

    #[test]
    fn human_render_shows_each_source_state_losses_and_last_loss() {
        let doc = StatusDoc::from_metrics(&sources_fixture());
        let human = render_human(&doc);
        let lines: Vec<&str> = human.lines().collect();
        let header = lines
            .iter()
            .position(|l| *l == "Sources:")
            .unwrap_or_else(|| panic!("no Sources section: {human}"));
        assert_eq!(
            &lines[header..header + 4],
            &[
                "Sources:",
                "  NAME                             STATE   LOST  LAST LOSS",
                "  file                             up         0  never",
                "  kiwi                             down       1  3s ago, no usable samples",
            ],
            "{human}"
        );
        // After the active-tracks block, before the uplink.
        let tracks = lines
            .iter()
            .position(|l| l.starts_with("  active tracks"))
            .unwrap();
        let uplink = lines
            .iter()
            .position(|l| l.starts_with("RBN uplink"))
            .unwrap();
        assert!(tracks < header && header < uplink, "{human}");
        assert!(human.lines().all(|l| l.chars().count() <= 80), "{human}");

        // The longest realistic row is exactly 80 columns.
        let mut wide = doc.clone();
        wide.sources = vec![SourceStatus {
            name: "a".repeat(32),
            healthy: false,
            losses_total: 999_999,
            last_loss_reason: Some("no usable samples".to_string()),
            last_loss_age_seconds: Some(1_000_000.0),
        }];
        let out = render_human(&wide);
        let row = out
            .lines()
            .find(|l| l.contains("1000000s ago"))
            .unwrap_or_else(|| panic!("{out}"));
        assert_eq!(row.chars().count(), 80, "{row:?}");

        // A daemon-supplied name or reason cannot reach the terminal raw.
        let mut hostile = doc;
        hostile.sources[0].name = "evil\u{1b}]0;pwned\u{7}\nforged".to_string();
        hostile.sources[1].last_loss_reason = Some("x\u{1b}[2J".to_string());
        let out = render_human(&hostile);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        assert!(out.contains(r"evil\u{1b}]0;pwned\u{7}\nforged"), "{out}");
        assert!(out.contains(r"3s ago, x\u{1b}[2J"), "{out}");
        assert_eq!(out.lines().count(), human.lines().count(), "{out:?}");
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
    fn active_tracks_reports_the_live_gauge_rather_than_a_null_placeholder() {
        // MAN-44 review CR-B: `manta-cli`'s poller feeds
        // `set_active_tracks` every 250ms (MAN-45), so `/status` and
        // `manta status` must show that count -- they used to hardcode
        // `None` and report "n/a" while `/metrics` reported the truth.
        let m = metrics_fixture();
        m.set_active_tracks(12);
        let doc = StatusDoc::from_metrics(&m);
        assert_eq!(doc.active_tracks, Some(12));
        assert!(render_human(&doc).contains("active tracks  12"));
    }

    #[test]
    fn a_status_doc_without_active_tracks_renders_n_a_rather_than_a_fabricated_zero() {
        // The field stays `Option` for the schema contract: a daemon that
        // sends no count at all must not be rendered as "0 tracks".
        let mut doc = StatusDoc::from_metrics(&metrics_fixture());
        doc.active_tracks = None;
        assert!(render_human(&doc).contains("active tracks  n/a"));
    }

    #[test]
    fn human_render_saturates_overflowing_client_counts() {
        // MAN-281 (Codex review, PR #95): `manta status --addr` can reach a
        // spoofed endpoint, and any `i64` is schema-valid, so the json/ws sum
        // must neither panic (debug) nor wrap negative (release).
        for (json, ws, want) in [
            (
                i64::MAX,
                1,
                "  telnet clients       0      json/ws clients  9223372036854775807",
            ),
            (2, 1, "  telnet clients       0      json/ws clients  3"),
        ] {
            let mut doc = StatusDoc::from_metrics(&metrics_fixture());
            doc.json_clients = json;
            doc.ws_clients = ws;
            let out = render_human(&doc);
            assert!(out.lines().any(|l| l == want), "{want:?} not in {out:?}");
        }
    }

    #[test]
    fn human_render_escapes_control_characters_in_a_daemon_supplied_target_label() {
        // Codex review, PR #95: `manta status --addr` can reach a spoofed
        // endpoint, and serde_json decodes `\u001b`/`\n` inside a JSON
        // string, so a label written verbatim could inject ANSI/OSC
        // sequences or forge an extra table row on the operator's terminal.
        let clean = render_human(&StatusDoc::from_metrics(&metrics_fixture()));
        let mut doc = StatusDoc::from_metrics(&metrics_fixture());
        let hostile = "evil:7000\u{1b}]0;pwned\u{7}\n  forged.example:7000 connected";
        doc.uplink.targets[0].label = hostile.to_string();

        let out = render_human(&doc);
        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "no control character but the renderer's own line breaks may reach the terminal: {out:?}"
        );
        assert_eq!(
            out.lines().count(),
            clean.lines().count(),
            "an embedded newline must not forge an extra row: {out:?}"
        );
        assert!(out.contains(r"evil:7000\u{1b}]0;pwned\u{7}\n  forged.example:7000 connected"));
        // The document keeps the original value: `--json` output is
        // serde_json-escaped already and stays faithful to what was sent.
        assert_eq!(doc.uplink.targets[0].label, hostile);
        assert!(doc.to_json().contains(r"evil:7000\u001b]0;pwned\u0007\n"));
    }

    #[test]
    fn json_output_escapes_del_and_c1_controls_that_serde_json_leaves_raw() {
        // serde_json \u-escapes only U+0000-U+001F, so DEL and the C1
        // controls (U+009B is a one-character CSI) in a spoofed endpoint's
        // label would reach the terminal raw through `manta status --json`.
        let mut doc = StatusDoc::from_metrics(&metrics_fixture());
        let hostile = "evil:7000\u{9b}2J\u{9d}0;pwned\u{9c}\u{7f}";
        doc.uplink.targets[0].label = hostile.to_string();

        let json = doc.to_json();
        assert!(
            !json.chars().any(|c| c.is_control() && c != '\n'),
            "no control character but the pretty-printer's line breaks may reach the terminal: {json:?}"
        );
        assert!(
            json.contains(r"evil:7000\u009b2J\u009d0;pwned\u009c\u007f"),
            "{json:?}"
        );
        let back: StatusDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.uplink.targets[0].label, hostile,
            "the decoded value must be unchanged"
        );
    }

    #[test]
    fn value_to_json_escapes_del_and_c1_in_a_field_status_doc_does_not_declare() {
        let mut value = serde_json::to_value(StatusDoc::from_metrics(&metrics_fixture())).unwrap();
        value["note"] = serde_json::json!("evil\u{9b}2J\u{9d}0;pwned\u{9c}\u{7f}");

        let json = value_to_json(&value);
        assert!(
            !json.chars().any(|c| c.is_control() && c != '\n'),
            "only the pretty-printer's line breaks may reach the terminal: {json:?}"
        );
        assert!(
            json.contains(r"evil\u009b2J\u009d0;pwned\u009c\u007f"),
            "{json:?}"
        );
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value, "the decoded value must be unchanged");
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
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
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
        let a = m.register_uplink_target("a.example:7000".to_string(), true);
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
        // with the runbook's own 29-character worked-example target (as
        // long as RBN-style hostnames get; MAN-90 replaced RBN's outbound
        // relay host there, which is not an uplink target), plus a 5-digit
        // sent count matching the runbook's own sample.
        let m = Metrics::new();
        let a = m.register_uplink_target("collector-01.example.net:7000".to_string(), true);
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
