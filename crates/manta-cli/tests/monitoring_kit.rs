//! MAN-129: the monitoring kit (docs/RUNBOOKS/monitoring/ and docs/RUNBOOKS/monitoring.md)
//! stays in step with the code: the dashboard and alert rules query only families `/metrics`
//! renders, the uplink alert and panel use the daemon's own flapping rule, the promtool tests
//! cover every alert firing and quiet, and the runbook has a section for every alert.

use manta_server::metrics::{FLAPPING_RECONNECTS, RECONNECT_WINDOW};
use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const KIT_DIR: &str = "docs/RUNBOOKS/monitoring";
const RUNBOOK: &str = "docs/RUNBOOKS/monitoring.md";
const RUNBOOK_URL: &str =
    "https://github.com/HagaleTechnologies/manta/blob/main/docs/RUNBOOKS/monitoring.md";
const ALERTS: &str = "manta-alerts.yml";
const ALERT_TESTS: &str = "manta-alerts.test.yml";
const PROMETHEUS: &str = "prometheus.yml";
const DASHBOARD: &str = "manta-dashboard.json";
const ALERT_NAMES: [&str; 4] = [
    "MantaNodeDown",
    "MantaNodeUnhealthy",
    "MantaNoSpots",
    "MantaUplinkDisconnected",
];
/// The four panels the ticket names.
const TICKET_PANELS: [&str; 4] = [
    "Spot rate",
    "Active tracks",
    "Source health",
    "Uplink status",
];
/// The node filter every dashboard selector carries.
const NODE_FILTER: &str = r#"instance=~"$instance""#;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
        // Windows checkouts may convert LF to CRLF; the tests below parse LF-delimited
        // line structure.
        .replace("\r\n", "\n")
}

fn kit(name: &str) -> String {
    doc(&format!("{KIT_DIR}/{name}"))
}

fn yaml(name: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::from_str(&kit(name)).unwrap_or_else(|e| panic!("{KIT_DIR}/{name}: {e}"))
}

/// The families crates/manta-server/src/metrics.rs renders (its `# TYPE` lines),
/// the same regex as docs_consistency.rs::metrics_reference_lists_every_rendered_family.
fn rendered_families() -> BTreeSet<String> {
    let src = doc("crates/manta-server/src/metrics.rs");
    let type_line = Regex::new(r"# TYPE (manta_[a-z0-9_]+) ").unwrap();
    let families: BTreeSet<String> = type_line
        .captures_iter(&src)
        .map(|c| c[1].to_string())
        .collect();
    assert!(
        !families.is_empty(),
        "no `# TYPE` lines found in metrics.rs"
    );
    families
}

/// `manta_*` names in `text` that metrics.rs does not render (`_bucket`/`_sum`/`_count`
/// stripped first, as field_node_runbook.rs does). Also asserts at least one was seen.
fn unrendered_metrics(text: &str) -> Vec<String> {
    let families = rendered_families();
    let token = Regex::new(r"\bmanta_[a-z0-9_]+").unwrap();
    let mut seen = 0;
    let mut missing = Vec::new();
    for m in token.find_iter(text) {
        let raw = m.as_str();
        let name = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| raw.strip_suffix(s))
            .unwrap_or(raw);
        seen += 1;
        if !families.contains(name) {
            missing.push(raw.to_string());
        }
    }
    assert!(seen > 0, "no manta_* metric in {text:?}");
    missing.sort();
    missing.dedup();
    missing
}

/// The metric names a PromQL expression selects: identifiers left once string
/// literals, label matchers, range/offset brackets, grouping clauses, function
/// calls and keywords are taken out.
fn selected_metrics(expr: &str) -> BTreeSet<String> {
    let strings = Regex::new(r#""[^"]*""#).unwrap();
    let braces = Regex::new(r"\{[^}]*\}").unwrap();
    let brackets = Regex::new(r"\[[^\]]*\]").unwrap();
    let grouping =
        Regex::new(r"\b(?:on|ignoring|by|without|group_left|group_right)\s*\([^)]*\)").unwrap();
    let mut s = strings.replace_all(expr, "\"\"").into_owned();
    s = braces.replace_all(&s, " ").into_owned();
    s = brackets.replace_all(&s, " ").into_owned();
    s = grouping.replace_all(&s, " ").into_owned();
    let ident = Regex::new(r"\b[a-zA-Z_][a-zA-Z0-9_:]*").unwrap();
    let keywords = ["and", "or", "unless", "offset", "bool"];
    ident
        .find_iter(&s)
        .filter(|m| !keywords.contains(&m.as_str()))
        .filter(|m| !s[m.end()..].trim_start().starts_with('('))
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Every alerting rule in manta-alerts.yml as (alert name, rule mapping), file order.
fn alert_rules() -> Vec<(String, serde_yaml_ng::Value)> {
    let doc = yaml(ALERTS);
    let groups = doc["groups"]
        .as_sequence()
        .unwrap_or_else(|| panic!("{ALERTS} has no `groups:` list"));
    groups
        .iter()
        .flat_map(|g| g["rules"].as_sequence().cloned().unwrap_or_default())
        .filter_map(|r| {
            r.get("alert")
                .and_then(|a| a.as_str())
                .map(|a| (a.to_string(), r.clone()))
        })
        .collect()
}

fn rule<'a>(rules: &'a [(String, serde_yaml_ng::Value)], name: &str) -> &'a serde_yaml_ng::Value {
    rules
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, r)| r)
        .unwrap_or_else(|| panic!("{ALERTS} has no alert {name}"))
}

/// The rule's `expr`, whitespace squashed to single spaces.
fn expr(rule: &serde_yaml_ng::Value) -> String {
    rule["expr"]
        .as_str()
        .unwrap_or_else(|| panic!("rule has no string `expr`: {rule:?}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Prometheus duration of one unit (`90s`, `10m`, `2h`) in seconds.
fn prom_seconds(d: &str) -> u64 {
    let re = Regex::new(r"^(\d+)([smh])$").unwrap();
    let c = re
        .captures(d)
        .unwrap_or_else(|| panic!("{d:?} is not a one-unit Prometheus duration"));
    let n: u64 = c[1].parse().expect("digits");
    n * match &c[2] {
        "s" => 1,
        "m" => 60,
        _ => 3600,
    }
}

fn rule_for(rule: &serde_yaml_ng::Value) -> Option<&str> {
    rule.get("for").and_then(|f| f.as_str())
}

#[test]
fn alert_rules_define_the_four_alerts() {
    let doc = yaml(ALERTS);
    let groups = doc["groups"].as_sequence().expect("groups list");
    assert_eq!(groups.len(), 1, "{ALERTS} must have exactly one group");
    assert_eq!(groups[0]["name"].as_str(), Some("manta"));
    let rules = alert_rules();
    let names: Vec<&str> = rules.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ALERT_NAMES, "{ALERTS} alert names, in file order");
    for (name, r) in &rules {
        assert!(!expr(r).is_empty(), "{name} has an empty expr");
        let want = match name.as_str() {
            "MantaNodeDown" | "MantaNodeUnhealthy" => "critical",
            _ => "warning",
        };
        assert_eq!(
            r["labels"]["severity"].as_str(),
            Some(want),
            "{name} severity (critical for down/unhealthy, warning otherwise)"
        );
        for key in ["summary", "description"] {
            let v = r["annotations"][key].as_str().unwrap_or("");
            assert!(!v.trim().is_empty(), "{name} has no annotations.{key}");
        }
        assert_eq!(
            r["annotations"]["runbook_url"].as_str(),
            Some(format!("{RUNBOOK_URL}#{}", name.to_lowercase()).as_str()),
            "{name}'s runbook_url must land on its section of {RUNBOOK}"
        );
    }
}

#[test]
fn alert_expressions_query_only_rendered_metrics() {
    let rules = alert_rules();
    let all: Vec<String> = rules.iter().map(|(_, r)| expr(r)).collect();
    let missing = unrendered_metrics(&all.join("\n"));
    assert!(
        missing.is_empty(),
        "{ALERTS} queries metrics crates/manta-server/src/metrics.rs does not render: {missing:?}"
    );
    for (name, r) in &rules {
        let e = expr(r);
        for metric in selected_metrics(&e) {
            if metric.starts_with("manta_") {
                continue;
            }
            assert!(
                name == "MantaNodeDown" && metric == "up",
                "{name} selects non-manta metric {metric:?} in {e:?}"
            );
            assert!(
                e.contains(r#"up{job="manta"}"#),
                "{name} must select up only as up{{job=\"manta\"}}: {e:?}"
            );
        }
    }
    assert!(
        selected_metrics(&expr(rule(&rules, "MantaNodeDown"))).contains("up"),
        "MantaNodeDown must alert on Prometheus's up series"
    );
}

#[test]
fn uplink_alert_is_the_complement_of_status_connected() {
    let rules = alert_rules();
    let r = rule(&rules, "MantaUplinkDisconnected");
    let e = expr(r);
    for needle in [
        "manta_uplink_target_enabled == 1".to_string(),
        "unless".to_string(),
        "manta_uplink_target_connected == 1".to_string(),
        format!("manta_uplink_target_recent_reconnects < {FLAPPING_RECONNECTS}"),
    ] {
        assert!(
            e.contains(&needle),
            "MantaUplinkDisconnected must contain {needle:?} (GET /status's `connected` \
             verdict is enabled, connected and fewer than FLAPPING_RECONNECTS recent \
             reconnects): {e:?}"
        );
    }
    let for_ = rule_for(r).expect("MantaUplinkDisconnected needs a `for`");
    assert!(
        prom_seconds(for_) > RECONNECT_WINDOW.as_secs(),
        "MantaUplinkDisconnected `for: {for_}` must exceed RECONNECT_WINDOW ({} s): a burst \
         of reconnects keeps recent_reconnects >= {FLAPPING_RECONNECTS} for the whole window \
         after the target has recovered, so a shorter `for` would fire on a recovered target",
        RECONNECT_WINDOW.as_secs()
    );
}

#[test]
fn no_spots_alert_looks_back_as_far_as_it_requires_uptime() {
    let rules = alert_rules();
    let r = rule(&rules, "MantaNoSpots");
    let e = expr(r);
    let offset = Regex::new(r"offset (\d+[smh])")
        .unwrap()
        .captures(&e)
        .unwrap_or_else(|| panic!("MantaNoSpots has no `offset`: {e:?}"))[1]
        .to_string();
    let uptime: u64 = Regex::new(r"manta_uptime_seconds >= (\d+)")
        .unwrap()
        .captures(&e)
        .unwrap_or_else(|| panic!("MantaNoSpots has no `manta_uptime_seconds >=` term: {e:?}"))[1]
        .parse()
        .expect("digits");
    assert_eq!(
        prom_seconds(&offset),
        uptime,
        "MantaNoSpots must require as much uptime as it looks back, or a restart inside the \
         window compares two processes' counts"
    );
    assert_eq!(
        prom_seconds(&offset),
        1800,
        "the ticket's 30 minutes without spots"
    );
    assert!(
        rule_for(r).is_none(),
        "MantaNoSpots has no `for`: it fires at the first evaluation after 30 silent minutes"
    );
}

#[test]
fn node_down_alert_matches_the_example_scrape_job() {
    let prom = yaml(PROMETHEUS);
    assert_eq!(
        prom["rule_files"]
            .as_sequence()
            .map(|s| s.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
        Some(vec![ALERTS]),
        "{PROMETHEUS} rule_files"
    );
    let scrapes = prom["scrape_configs"]
        .as_sequence()
        .expect("scrape_configs list");
    assert_eq!(scrapes.len(), 1, "{PROMETHEUS} has exactly one scrape job");
    let job = scrapes[0]["job_name"].as_str().expect("job_name");
    let rules = alert_rules();
    let down = expr(rule(&rules, "MantaNodeDown"));
    assert!(
        down.contains(&format!(r#"up{{job="{job}"}}"#)),
        "MantaNodeDown must match the example scrape job {job:?}: {down:?}"
    );
    let target = scrapes[0]["static_configs"][0]["targets"][0]
        .as_str()
        .expect("first static target");
    let metrics_md = doc("docs/METRICS.md");
    let published = Regex::new(r#"targets: \["([^"]+)"\]"#)
        .unwrap()
        .captures(&metrics_md)
        .expect("docs/METRICS.md's scrape config has a target")[1]
        .to_string();
    assert_eq!(
        target, published,
        "{PROMETHEUS}'s target must match docs/METRICS.md's scrape config"
    );
    for name in ["MantaNodeDown", "MantaNodeUnhealthy"] {
        assert_eq!(
            rule_for(rule(&rules, name)),
            Some("2m"),
            "{name} waits 2 minutes, longer than a systemd restart"
        );
    }
}

#[test]
fn promtool_tests_cover_every_alert_firing_and_quiet() {
    let t = yaml(ALERT_TESTS);
    assert_eq!(
        t["rule_files"]
            .as_sequence()
            .map(|s| s.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
        Some(vec![ALERTS]),
        "{ALERT_TESTS} rule_files"
    );
    let cases: Vec<&serde_yaml_ng::Value> = t["tests"]
        .as_sequence()
        .expect("tests list")
        .iter()
        .flat_map(|test| {
            test.get("alert_rule_test")
                .and_then(|a| a.as_sequence())
                .map(|s| s.iter().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    for name in ALERT_NAMES {
        let exp_lens: Vec<usize> = cases
            .iter()
            .filter(|c| c["alertname"].as_str() == Some(name))
            .map(|c| c["exp_alerts"].as_sequence().map_or(0, |s| s.len()))
            .collect();
        assert!(
            exp_lens.iter().any(|&n| n > 0),
            "{ALERT_TESTS} has no case where {name} fires"
        );
        assert!(
            exp_lens.contains(&0),
            "{ALERT_TESTS} has no case where {name} stays quiet"
        );
    }
}

fn promtool() -> Option<String> {
    std::env::var("PROMTOOL")
        .ok()
        .into_iter()
        .chain(std::iter::once("promtool".to_string()))
        .find(|p| {
            Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
}

#[test]
fn promtool_checks_and_tests_the_kit_when_installed() {
    let Some(promtool) = promtool() else {
        eprintln!("skipping: promtool not found (set PROMTOOL=/path/to/promtool)");
        return;
    };
    let dir = repo_root().join(KIT_DIR);
    for args in [
        ["check", "rules", ALERTS],
        ["test", "rules", ALERT_TESTS],
        ["check", "config", PROMETHEUS],
    ] {
        let out = Command::new(&promtool)
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap_or_else(|e| panic!("running {promtool} {args:?}: {e}"));
        assert!(
            out.status.success(),
            "{promtool} {} failed in {KIT_DIR}:\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn dashboard() -> serde_json::Value {
    serde_json::from_str(&kit(DASHBOARD)).unwrap_or_else(|e| panic!("{KIT_DIR}/{DASHBOARD}: {e}"))
}

fn all_panels(d: &serde_json::Value) -> &Vec<serde_json::Value> {
    d["panels"]
        .as_array()
        .unwrap_or_else(|| panic!("{DASHBOARD} has no `panels` list"))
}

/// Every non-row panel, in file order.
fn panels(d: &serde_json::Value) -> Vec<&serde_json::Value> {
    all_panels(d)
        .iter()
        .filter(|p| p["type"].as_str() != Some("row"))
        .collect()
}

fn panel_titled<'a>(d: &'a serde_json::Value, title: &str) -> &'a serde_json::Value {
    panels(d)
        .into_iter()
        .find(|p| p["title"].as_str() == Some(title))
        .unwrap_or_else(|| panic!("{DASHBOARD} has no panel titled {title:?}"))
}

fn panel_exprs(p: &serde_json::Value) -> Vec<String> {
    p["targets"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t["expr"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Every target `expr` in the dashboard.
fn exprs(d: &serde_json::Value) -> Vec<String> {
    panels(d).into_iter().flat_map(panel_exprs).collect()
}

/// The colour Grafana's absolute thresholds give `v`: the last step whose value <= v
/// (the first step's null value is minus infinity).
fn threshold_colour(steps: &[serde_json::Value], v: f64) -> String {
    let mut colour = None;
    for step in steps {
        let reached = match step["value"].as_f64() {
            None => true,
            Some(at) => at <= v,
        };
        if reached {
            colour = step["color"].as_str();
        }
    }
    colour
        .unwrap_or_else(|| panic!("no threshold step covers {v} in {steps:?}"))
        .to_string()
}

/// Value mappings as (value, text, colour), from every `type: value` mapping.
fn value_mappings(p: &serde_json::Value) -> Vec<(f64, String, String)> {
    let mut out = Vec::new();
    for m in p["fieldConfig"]["defaults"]["mappings"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if m["type"].as_str() != Some("value") {
            continue;
        }
        for (k, o) in m["options"].as_object().into_iter().flatten() {
            let v: f64 = k
                .parse()
                .unwrap_or_else(|_| panic!("mapping key {k:?} is not a number"));
            out.push((
                v,
                o["text"].as_str().unwrap_or("").to_string(),
                o["color"].as_str().unwrap_or("").to_string(),
            ));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Every value under a `datasource` key, anywhere in the JSON.
fn datasource_refs<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                if k == "datasource" {
                    out.push(child);
                }
                datasource_refs(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                datasource_refs(child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn dashboard_imports_without_placeholders() {
    let d = dashboard();
    assert_eq!(d["uid"].as_str(), Some("manta-node"));
    assert_eq!(d["title"].as_str(), Some("manta node"));
    assert!(
        d["id"].is_null(),
        "id must be null so import creates a new dashboard"
    );
    assert_eq!(
        d["schemaVersion"].as_u64(),
        Some(39),
        "schemaVersion 39 is Grafana 10.4, the oldest version the runbook supports"
    );
    for key in ["__inputs", "__requires"] {
        assert!(
            d.get(key).is_none(),
            "{DASHBOARD} must not carry {key}: file provisioning does not resolve it"
        );
    }
    assert!(
        !kit(DASHBOARD).contains("${DS_"),
        "{DASHBOARD} must not carry ${{DS_…}} import placeholders"
    );
}

#[test]
fn dashboard_picks_its_prometheus_source_by_variable() {
    let d = dashboard();
    let vars = d["templating"]["list"].as_array().expect("templating.list");
    let var = |name: &str| {
        vars.iter()
            .find(|v| v["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("{DASHBOARD} has no ${name} variable"))
    };
    let ds = var("datasource");
    assert_eq!(ds["type"].as_str(), Some("datasource"));
    assert_eq!(ds["query"].as_str(), Some("prometheus"));
    let node = var("instance");
    assert_eq!(node["type"].as_str(), Some("query"));
    assert_eq!(node["multi"].as_bool(), Some(true));
    assert_eq!(node["includeAll"].as_bool(), Some(true));
    assert_eq!(
        node["definition"].as_str(),
        Some("label_values(manta_build_info, instance)")
    );
    let mut refs = Vec::new();
    datasource_refs(&d, &mut refs);
    let prometheus: Vec<_> = refs
        .into_iter()
        .filter(|r| r["type"].as_str() == Some("prometheus"))
        .collect();
    assert!(
        !prometheus.is_empty(),
        "{DASHBOARD} names no Prometheus data source"
    );
    for r in prometheus {
        assert_eq!(
            r["uid"].as_str(),
            Some("${datasource}"),
            "every Prometheus data source must be the $datasource variable, not a fixed uid"
        );
    }
}

#[test]
fn dashboard_has_the_panels_the_ticket_names() {
    let d = dashboard();
    for title in TICKET_PANELS {
        let matching: Vec<_> = panels(&d)
            .into_iter()
            .filter(|p| p["title"].as_str() == Some(title))
            .collect();
        assert!(!matching.is_empty(), "{DASHBOARD} has no {title:?} panel");
        for p in matching {
            assert!(
                panel_exprs(p).iter().any(|e| !e.trim().is_empty()),
                "{title:?} has no query"
            );
        }
    }
    let rows: Vec<&str> = all_panels(&d)
        .iter()
        .filter(|p| p["type"].as_str() == Some("row"))
        .filter_map(|p| p["title"].as_str())
        .collect();
    assert_eq!(
        rows,
        [
            "Overview",
            "Spots",
            "Decoder",
            "Input source",
            "RBN uplink",
            "Clients"
        ]
    );
    let overview: Vec<&str> = all_panels(&d)
        .iter()
        .skip(1)
        .take_while(|p| p["type"].as_str() != Some("row"))
        .filter_map(|p| p["title"].as_str())
        .collect();
    assert_eq!(
        overview,
        [
            "Health",
            "Uptime",
            "Restarts",
            "Spots / min",
            "Active tracks",
            "Version"
        ]
    );
}

#[test]
fn dashboard_queries_only_rendered_metrics_for_the_selected_nodes() {
    let d = dashboard();
    let all = exprs(&d);
    assert!(!all.is_empty(), "{DASHBOARD} has no queries");
    let missing = unrendered_metrics(&all.join("\n"));
    assert!(
        missing.is_empty(),
        "{DASHBOARD} queries metrics crates/manta-server/src/metrics.rs does not render: {missing:?}"
    );
    let selector = Regex::new(r"manta_[a-z0-9_]+(\{[^}]*\})?").unwrap();
    for e in &all {
        for m in selector.find_iter(e) {
            assert!(
                m.as_str().contains(NODE_FILTER),
                "{:?} in {e:?} does not filter on the Node variable ({NODE_FILTER})",
                m.as_str()
            );
        }
        for metric in selected_metrics(e) {
            assert!(
                metric.starts_with("manta_"),
                "{DASHBOARD} selects non-manta metric {metric:?} in {e:?}"
            );
        }
    }
}

#[test]
fn uplink_status_panel_is_the_expression_promtool_tests() {
    let d = dashboard();
    let p = panel_titled(&d, "Uplink status");
    let targets = panel_exprs(p);
    assert_eq!(targets.len(), 1, "Uplink status has one query");
    let e = targets[0].replace(&format!("{{{NODE_FILTER}}}"), "");
    let tests = yaml(ALERT_TESTS);
    let tested: Vec<&str> = tests["tests"]
        .as_sequence()
        .expect("tests list")
        .iter()
        .flat_map(|t| {
            t.get("promql_expr_test")
                .and_then(|p| p.as_sequence())
                .map(|s| s.iter().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .filter_map(|c| c["expr"].as_str())
        .collect();
    assert!(
        tested.contains(&e.as_str()),
        "Uplink status's query (node filter removed) is not a promql_expr_test in \
         {ALERT_TESTS}:\n{e}\ntested: {tested:#?}"
    );
    assert!(
        e.contains(&format!(">= bool {FLAPPING_RECONNECTS}")),
        "Uplink status must flag flapping at FLAPPING_RECONNECTS: {e:?}"
    );
    let mapped: Vec<(f64, String)> = value_mappings(p)
        .into_iter()
        .map(|(v, t, _)| (v, t.to_lowercase()))
        .collect();
    let want = [
        (0.0, "disabled"),
        (1.0, "down"),
        (2.0, "flapping"),
        (3.0, "connected"),
    ];
    assert_eq!(
        mapped,
        want.iter()
            .map(|(v, t)| (*v, t.to_string()))
            .collect::<Vec<_>>(),
        "Uplink status mappings"
    );
    let status_doc = doc("crates/manta-server/src/status_doc.rs");
    for (_, label) in want {
        assert!(
            status_doc.contains(&format!("=> \"{label}\"")),
            "status_doc.rs's target_health_label has no {label:?}: the panel's states must \
             be the ones GET /status reports"
        );
    }
}

#[test]
fn mapped_panels_colour_each_state_one_way() {
    let d = dashboard();
    for p in panels(&d) {
        let title = p["title"].as_str().unwrap_or("?");
        let defaults = &p["fieldConfig"]["defaults"];
        let mode = defaults["color"]["mode"].as_str();
        if p["type"].as_str() == Some("state-timeline") {
            assert_eq!(
                mode,
                Some("fixed"),
                "state timeline {title:?} must use color.mode \"fixed\": Grafana 13.2.3 \
                 ignores value-mapping colours on a state timeline in thresholds mode and \
                 draws every state in the base threshold colour"
            );
            assert!(
                defaults.get("noValue").is_none(),
                "state timeline {title:?} must not set noValue: Grafana 13.2.3 shows it as a \
                 legend entry next to real states"
            );
            continue;
        }
        let mapped = value_mappings(p);
        if mapped.is_empty() || mode == Some("fixed") {
            continue;
        }
        assert_eq!(
            mode,
            Some("thresholds"),
            "mapped panel {title:?} must use fixed or thresholds colour mode"
        );
        let steps = defaults["thresholds"]["steps"]
            .as_array()
            .unwrap_or_else(|| panic!("mapped panel {title:?} has no threshold steps"));
        for (v, text, colour) in mapped {
            assert_eq!(
                threshold_colour(steps, v),
                colour,
                "{title:?}: {text} ({v}) has mapping colour {colour} but a different \
                 threshold colour; Grafana 13.2.3 may draw either, so they must agree"
            );
        }
    }
}

#[test]
fn dashboard_layout_is_tidy() {
    let d = dashboard();
    let mut ids = BTreeSet::new();
    for p in all_panels(&d) {
        let id = p["id"].as_u64().expect("panel id");
        assert!(ids.insert(id), "duplicate panel id {id}");
        let g = &p["gridPos"];
        let (x, w) = (g["x"].as_u64().unwrap(), g["w"].as_u64().unwrap());
        assert!(x + w <= 24, "panel {id} overflows the 24-column grid");
    }
    let boxes: Vec<(&str, [u64; 4])> = panels(&d)
        .into_iter()
        .map(|p| {
            let g = &p["gridPos"];
            let v = |k: &str| g[k].as_u64().unwrap();
            (
                p["title"].as_str().unwrap_or("?"),
                [v("x"), v("y"), v("w"), v("h")],
            )
        })
        .collect();
    for (i, (a, [ax, ay, aw, ah])) in boxes.iter().enumerate() {
        for (b, [bx, by, bw, bh]) in &boxes[i + 1..] {
            let overlap = ax < &(bx + bw) && bx < &(ax + aw) && ay < &(by + bh) && by < &(ay + ah);
            assert!(!overlap, "panels {a:?} and {b:?} overlap");
        }
    }
    for p in panels(&d) {
        let title = p["title"].as_str().unwrap_or("?");
        assert!(
            p["description"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()),
            "panel {title:?} has no description (its ⓘ tooltip)"
        );
    }
}

#[test]
fn runbook_has_a_section_for_every_alert() {
    let runbook = doc(RUNBOOK);
    for name in ALERT_NAMES {
        assert!(
            runbook
                .lines()
                .any(|l| l.trim_end() == format!("### {name}")),
            "{RUNBOOK} has no `### {name}` section; the alert's runbook_url points at \
             #{}",
            name.to_lowercase()
        );
    }
    assert_eq!(
        dashboard()["links"][0]["url"].as_str(),
        Some(RUNBOOK_URL),
        "the dashboard's first link is the monitoring runbook"
    );
}

#[test]
fn runbook_names_every_kit_file_and_the_ticket_panels() {
    let runbook = doc(RUNBOOK);
    let mut files: Vec<String> = fs::read_dir(repo_root().join(KIT_DIR))
        .expect("kit directory")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "{KIT_DIR} is empty");
    for f in files {
        assert!(runbook.contains(&f), "{RUNBOOK} does not name kit file {f}");
    }
    for title in TICKET_PANELS {
        assert!(
            runbook.contains(title),
            "{RUNBOOK} does not name the {title:?} panel"
        );
    }
}

#[test]
fn runbook_cites_only_rendered_metrics() {
    let runbook = doc(RUNBOOK);
    let metrics_src = doc("crates/manta-server/src/metrics.rs");
    let token = Regex::new(r"\bmanta_[a-z0-9_]+").unwrap();
    let mut missing = Vec::new();
    let mut seen = 0;
    for m in token.find_iter(&runbook) {
        let raw = m.as_str();
        let name = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| raw.strip_suffix(s))
            .unwrap_or(raw);
        seen += 1;
        if !metrics_src.contains(&format!("# TYPE {name} ")) {
            missing.push(raw.to_string());
        }
    }
    assert!(seen > 0, "{RUNBOOK} cites no manta_* metric at all");
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "{RUNBOOK} cites metrics crates/manta-server/src/metrics.rs does not render: {missing:?}"
    );
}

#[test]
fn runbook_relative_links_resolve() {
    let runbook = doc(RUNBOOK);
    let base = repo_root()
        .join(RUNBOOK)
        .parent()
        .expect("runbook has a directory")
        .to_path_buf();
    let link = Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    let mut broken = Vec::new();
    for cap in link.captures_iter(&runbook) {
        let target = &cap[1];
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
            || target.starts_with('#')
        {
            continue;
        }
        let path = target.split('#').next().unwrap_or(target);
        if !base.join(path).exists() {
            broken.push(target.to_string());
        }
    }
    assert!(
        broken.is_empty(),
        "{RUNBOOK} links to files that do not exist (relative to {}): {broken:?}",
        base.display()
    );
}
