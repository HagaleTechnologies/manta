//! MAN-75: the shipped operator artifacts (`manta.example.toml`, the
//! systemd unit, the launchd plist, `docker-compose.yml`) are executable
//! documentation, so CI checks them against the code they describe.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

const EXAMPLE_TOML: &str = "manta.example.toml";
const SERVICE: &str = "packaging/systemd/manta.service";
const PLIST: &str = "packaging/launchd/com.hagaletechnologies.manta.plist";
const COMPOSE: &str = "docker-compose.yml";

/// The all-defaults config: what the shipped file must be equivalent to.
fn defaults() -> manta_server::config::ServerConfig {
    toml::from_str(r#"station_callsign = "N0CALL""#).unwrap()
}

/// serde's `deny_unknown_fields` error enumerates every field of the
/// struct, which is the only runtime field list these `Deserialize`-only
/// types expose. Adding a config key therefore fails
/// `example_toml_documents_every_config_key` below until the example file
/// documents it.
fn field_names(err: &str) -> Vec<String> {
    let tail = err.split("expected one of ").nth(1).unwrap_or_else(|| {
        panic!("expected a deny_unknown_fields error listing fields, got:\n{err}")
    });
    tail.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.to_string())
        .collect()
}

fn server_config_fields() -> Vec<String> {
    let e = toml::from_str::<manta_server::config::ServerConfig>(
        "station_callsign = \"N0CALL\"\nzz_unknown_probe = 1\n",
    )
    .unwrap_err();
    field_names(&e.to_string())
}

fn rbn_uplink_fields() -> Vec<String> {
    let e = toml::from_str::<manta_server::config::DaemonConfigFile>(
        "[server]\nstation_callsign = \"N0CALL\"\n\n[[rbn_uplink]]\nenabled = true\ntarget_host = \"h\"\ntarget_port = 1\nzz_unknown_probe = 1\n",
    )
    .unwrap_err();
    field_names(&e.to_string())
}

/// Splits the example file at the `[[rbn_uplink]]` block: only `[server]`
/// keys are subject to the uncomment-is-a-no-op invariant (an uplink block
/// has required keys with no defaults at all).
fn server_section(text: &str) -> String {
    text.split("[[rbn_uplink]]").next().unwrap().to_string()
}

#[test]
fn example_toml_parses_and_is_defaults_plus_a_callsign() {
    let cfg: manta_server::config::DaemonConfigFile = toml::from_str(&read(EXAMPLE_TOML))
        .expect("manta.example.toml must parse as the real daemon config");
    assert_eq!(cfg.server, defaults(), "shipped file must be pure defaults");
    assert_eq!(cfg.server.station_callsign, "N0CALL");
    assert!(
        cfg.rbn_uplink.is_empty(),
        "the shipped file must not configure an uplink"
    );
}

#[test]
fn uncommenting_every_shown_default_changes_nothing() {
    let text = read(EXAMPLE_TOML);
    let mut uncommented = String::new();
    let mut count = 0usize;
    for line in server_section(&text).lines() {
        let t = line.trim_start();
        let body = t.strip_prefix("# ").unwrap_or("");
        let is_key_line = body.split_once(" = ").is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        });
        if is_key_line {
            uncommented.push_str(body);
            count += 1;
        } else {
            uncommented.push_str(line);
        }
        uncommented.push('\n');
    }
    assert!(
        count >= 9,
        "expected the commented default lines, found {count}"
    );

    let cfg = toml::from_str::<manta_server::config::DaemonConfigFile>(&uncommented)
        .expect("uncommented example must still parse")
        .server;
    let d = defaults();
    assert_eq!(cfg.bind_addr, d.bind_addr);
    assert_eq!(cfg.telnet_port, d.telnet_port);
    assert_eq!(cfg.json_port, d.json_port);
    assert_eq!(cfg.metrics_port, d.metrics_port);
    // The five Option fields are `None` when omitted; the value each
    // listener then applies lives in that listener's own public constant.
    assert_eq!(
        cfg.telnet_max_connections_per_ip,
        Some(manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP)
    );
    assert_eq!(
        cfg.json_max_connections_per_ip,
        Some(manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP)
    );
    assert_eq!(
        cfg.metrics_max_connections_per_ip,
        Some(manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP)
    );
    assert_eq!(
        cfg.telnet_max_commands_per_ip,
        Some(manta_server::telnet::MAX_TELNET_COMMANDS)
    );
    assert_eq!(
        cfg.json_max_pings_per_ip,
        Some(manta_server::json_stream::MAX_INBOUND_PINGS)
    );
}

#[test]
fn example_toml_documents_every_config_key() {
    let text = read(EXAMPLE_TOML);
    for field in server_config_fields()
        .into_iter()
        .chain(rbn_uplink_fields())
    {
        assert!(
            text.contains(&format!("{field} = ")),
            "manta.example.toml does not document config key `{field}`"
        );
    }
}

#[test]
fn example_toml_shows_the_real_dry_run_default() {
    let code_default: manta_server::config::DaemonConfigFile = toml::from_str(
        "[server]\nstation_callsign = \"N0CALL\"\n\n[[rbn_uplink]]\nenabled = true\ntarget_host = \"h\"\ntarget_port = 1\n",
    )
    .unwrap();
    let shown = read(EXAMPLE_TOML)
        .lines()
        .find_map(|l| {
            l.trim_start()
                .strip_prefix("# dry_run = ")
                .map(str::to_string)
        })
        .expect("example must show a dry_run line");
    assert_eq!(
        shown.trim(),
        code_default.rbn_uplink[0].dry_run.to_string(),
        "the example's dry_run value must be the code's real default"
    );
}

// ---------------------------------------------------------------- systemd

fn service_directives() -> Vec<(String, String, String)> {
    let text = read(SERVICE);
    let mut section = String::new();
    let mut out = Vec::new();
    let mut pending: Option<(String, String)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = pending.take() {
            let cont = line.strip_suffix('\\');
            let v = format!("{v} {}", cont.unwrap_or(line).trim());
            if cont.is_some() {
                pending = Some((k, v));
            } else {
                out.push((section.clone(), k, v));
            }
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        let (k, v) = line.split_once('=').expect("unit line must be key=value");
        if let Some(head) = v.strip_suffix('\\') {
            pending = Some((k.to_string(), head.trim().to_string()));
        } else {
            out.push((section.clone(), k.to_string(), v.to_string()));
        }
    }
    assert!(pending.is_none(), "unit ends on a dangling continuation");
    out
}

#[test]
fn systemd_unit_carries_the_required_directives() {
    let d = service_directives();
    let get = |sec: &str, key: &str| -> String {
        d.iter()
            .find(|(s, k, _)| s == sec && k == key)
            .unwrap_or_else(|| panic!("[{sec}] {key}= missing from {SERVICE}"))
            .2
            .clone()
    };
    assert_eq!(get("Service", "Restart"), "always");
    assert_eq!(get("Service", "TimeoutStopSec"), "30");
    assert_eq!(get("Service", "DynamicUser"), "yes");
    // Until manta handles SIGTERM itself, systemd must send the signal it
    // does handle -- the same workaround the Dockerfile applies with
    // STOPSIGNAL SIGINT.
    assert_eq!(get("Service", "KillSignal"), "SIGINT");
    assert_eq!(get("Service", "ConfigurationDirectory"), "manta");
    // StartLimitIntervalSec belongs to [Unit] in modern systemd; in
    // [Service] it is a deprecated alias that logs a warning.
    assert_eq!(get("Unit", "StartLimitIntervalSec"), "0");
    assert_eq!(get("Install", "WantedBy"), "multi-user.target");

    let exec = get("Service", "ExecStart");
    assert!(
        exec.contains("--server-config /etc/manta/manta.toml"),
        "ExecStart must read the config from the ConfigurationDirectory: {exec}"
    );
}

// ---------------------------------------------------------------- launchd

#[test]
fn launchd_plist_has_no_double_hyphen_inside_a_comment() {
    // XML forbids `--` inside `<!-- -->`; a flag name in a plist comment
    // silently makes the whole file unparseable to launchd.
    let text = read(PLIST);
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<!--") {
        let body_start = start + 4;
        let end = rest[body_start..]
            .find("-->")
            .expect("unterminated XML comment");
        let body = &rest[body_start..body_start + end];
        assert!(
            !body.contains("--"),
            "XML comment contains a double hyphen, which makes the plist \
             unparseable:\n{body}"
        );
        rest = &rest[body_start + end + 3..];
    }
}

#[test]
fn launchd_plist_carries_the_required_keys() {
    let text = read(PLIST);
    for key in [
        "Label",
        "ProgramArguments",
        "RunAtLoad",
        "KeepAlive",
        "ExitTimeOut",
    ] {
        assert!(
            text.contains(&format!("<key>{key}</key>")),
            "{PLIST} is missing <key>{key}</key>"
        );
    }
    assert!(
        text.contains("<key>ExitTimeOut</key>\n    <integer>30</integer>"),
        "ExitTimeOut must be 30 s, matching TimeoutStopSec/stop_grace_period"
    );
    assert!(
        text.contains("<string>com.hagaletechnologies.manta</string>"),
        "Label must match this file's own name, as launchd expects"
    );
}

// ---------------------------------------------------------------- compose

#[test]
fn compose_sets_the_grace_period_and_leaves_the_stop_signal_alone() {
    let text = read(COMPOSE);
    let directive = |k: &str| {
        text.lines()
            .map(str::trim)
            .find(|l| l.starts_with(&format!("{k}:")))
            .map(|l| l.split_once(':').unwrap().1.trim().to_string())
    };
    assert_eq!(directive("stop_grace_period").as_deref(), Some("30s"));
    // The image already sets STOPSIGNAL SIGINT; an override here would
    // reintroduce the abrupt SIGTERM kill.
    assert!(
        directive("stop_signal").is_none(),
        "docker-compose.yml must not override the image's STOPSIGNAL"
    );
    assert!(
        text.contains(":/etc/manta/manta.toml:ro"),
        "the config must be mounted read-only at the path `command:` reads"
    );
}

// ------------------------------------------------ cross-artifact coherence

/// Every long flag the three shipped command lines actually use must be a
/// real `manta listen` flag in a DEFAULT-feature build -- the units are
/// useless if a flag rename silently orphans them.
#[test]
fn every_shipped_command_line_flag_exists() {
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();

    let exec = service_directives()
        .into_iter()
        .find(|(s, k, _)| s == "Service" && k == "ExecStart")
        .unwrap()
        .2;
    let plist = read(PLIST);
    let compose = read(COMPOSE);

    let mut used: Vec<String> = Vec::new();
    used.extend(exec.split_whitespace().map(str::to_string));
    used.extend(
        plist
            .lines()
            .filter_map(|l| l.trim().strip_prefix("<string>"))
            .filter_map(|l| l.strip_suffix("</string>"))
            .map(str::to_string),
    );
    used.extend(
        compose
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("- "))
            .map(|l| l.trim_matches('"').to_string()),
    );

    let flags: Vec<String> = used.into_iter().filter(|t| t.starts_with("--")).collect();
    assert!(
        flags.len() >= 9,
        "expected flags from all three files, got {flags:?}"
    );
    for f in flags {
        // Match clap's own option-definition line only (trimmed line starts
        // with the flag itself), not the whole help blob -- several flags
        // are cross-referenced in *other* options' description prose (e.g.
        // "Required with --server-config"), which would keep a stale
        // substring alive after the option itself is renamed away.
        let is_option_definition_line = |l: &str| {
            let t = l.trim_start();
            t == f || t.starts_with(&format!("{f} "))
        };
        assert!(
            help.lines().any(is_option_definition_line),
            "`{f}` is used by a shipped example but is not a `manta listen` \
             flag in a default-feature build"
        );
    }
}

/// The three stop budgets must all clear manta's own worst-case drain:
/// SHUTDOWN_DRAIN_DEADLINE (25 s) + the runtime's 2 s hard cutoff.
#[test]
fn every_stop_budget_clears_the_drain_window() {
    const WORST_CASE_DRAIN_SECS: u64 = 25 + 2;
    let service: u64 = service_directives()
        .into_iter()
        .find(|(s, k, _)| s == "Service" && k == "TimeoutStopSec")
        .unwrap()
        .2
        .parse()
        .unwrap();
    assert!(service > WORST_CASE_DRAIN_SECS, "TimeoutStopSec={service}");
    assert!(read(PLIST).contains("<integer>30</integer>"));
    assert!(read(COMPOSE).contains("stop_grace_period: 30s"));
}
