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
/// launchd has no size cap or rotation for `StandardOutPath`, so the
/// rotation policy for the service agent's log ships as its own agent.
const LOGROTATE_PLIST: &str = "packaging/launchd/com.hagaletechnologies.manta-logrotate.plist";
/// Referenced by relative path from the shipped example config and the
/// Compose file, so it has to be in the archive alongside them.
const EXPOSURE_RUNBOOK: &str = "docs/RUNBOOKS/network-exposure.md";

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
    // silently makes the whole file unparseable to launchd. Both shipped
    // plists are checked -- the rotation agent is as easy to break this way
    // as the service agent.
    for plist in [PLIST, LOGROTATE_PLIST] {
        let text = read(plist);
        let mut rest = text.as_str();
        while let Some(start) = rest.find("<!--") {
            let body_start = start + 4;
            let end = rest[body_start..]
                .find("-->")
                .expect("unterminated XML comment");
            let body = &rest[body_start..body_start + end];
            assert!(
                !body.contains("--"),
                "{plist}: XML comment contains a double hyphen, which makes \
                 the plist unparseable:\n{body}"
            );
            rest = &rest[body_start + end + 3..];
        }
    }
}

/// The value of a `<key>k</key>` immediately followed by a `<string>`, as
/// the shipped plists format it.
fn plist_string(text: &str, key: &str) -> Option<String> {
    let tail = text.split(&format!("<key>{key}</key>")).nth(1)?;
    let v = tail.trim_start().strip_prefix("<string>")?;
    Some(v.split("</string>").next()?.to_string())
}

/// launchd never rotates or caps the file `StandardOutPath` names, and
/// manta's console stream is per decoded character, so the shipped 24/7
/// agent needs a rotation policy shipped with it -- pointed at the same
/// log the service agent actually writes, or it rotates nothing.
#[test]
fn a_rotation_policy_ships_for_the_launchagent_log() {
    let service = read(PLIST);
    let out =
        plist_string(&service, "StandardOutPath").expect("service plist must set StandardOutPath");
    let err = plist_string(&service, "StandardErrorPath")
        .expect("service plist must set StandardErrorPath");
    assert_eq!(out, err, "both streams are expected to share one file");

    let rotate = read(LOGROTATE_PLIST);
    assert_eq!(
        plist_string(&rotate, "Label").as_deref(),
        Some("com.hagaletechnologies.manta-logrotate"),
        "Label must match this file's own name, as launchd expects"
    );
    assert!(
        rotate.contains(&out),
        "{LOGROTATE_PLIST} does not act on {out}, the log the service agent \
         actually writes"
    );
    assert!(
        rotate.contains("<key>StartInterval</key>"),
        "the rotation agent must run periodically, not once"
    );

    // Rotating by rename would leave the renamed file growing: manta holds
    // the descriptor launchd opened for it for its whole lifetime, so the
    // policy has to truncate that same inode in place.
    let script = rotate
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .expect("rotation agent must set ProgramArguments")
        .split("</array>")
        .next()
        .unwrap();
    assert!(
        !script.contains("mv "),
        "rotation must truncate the log in place, not rename it: the \
         running manta keeps writing to the renamed inode:\n{script}"
    );
    assert!(
        script.contains(&out),
        "the rotation script itself must act on {out}, not just mention it \
         in a comment:\n{script}"
    );

    let readme = read("packaging/README.md");
    assert!(
        readme.contains("com.hagaletechnologies.manta-logrotate.plist"),
        "packaging/README.md never tells the operator to install the \
         rotation agent, so the shipped file is inert"
    );
}

/// A `gui/` LaunchAgent is a login-session job: it does not start at boot
/// and is torn down at logout. An operator promised a 24/7 service has to
/// be told that, and given the LaunchDaemon route for the sources that do
/// not need per-user TCC audio access.
#[test]
fn macos_docs_address_the_login_session_limitation() {
    let readme = read("packaging/README.md");
    let section = readme
        .split("## Installing on macOS")
        .nth(1)
        .expect("packaging/README.md must have a macOS install section");
    let section = section.split("\n## ").next().unwrap();
    for needle in [
        "LaunchDaemon",
        "launchctl bootstrap system",
        "/Library/LaunchDaemons/",
        "automatic login",
    ] {
        assert!(
            section.contains(needle),
            "the macOS section never mentions `{needle}`, so the \
             LaunchAgent's login-session limitation is undocumented"
        );
    }
}

/// `launchctl kill` only DELIVERS the signal; it returns while manta is
/// still draining. A documented stop sequence that boots the job out at
/// that moment SIGTERMs the still-draining process -- the exact abrupt
/// kill the sequence exists to avoid -- so the wait between the two is
/// load-bearing.
#[test]
fn macos_stop_sequence_waits_for_the_drain_before_bootout() {
    let readme = read("packaging/README.md");
    let section = readme
        .split("## Installing on macOS")
        .nth(1)
        .expect("packaging/README.md must have a macOS install section");
    let section = section.split("\n## ").next().unwrap();
    let kill = section
        .find("launchctl kill SIGINT")
        .expect("the macOS section must document the SIGINT stop");
    let bootout = section[kill..]
        .find("launchctl bootout")
        .expect("the macOS section must document the bootout that follows")
        + kill;
    let between = &section[kill..bootout];
    assert!(
        between.contains("launchctl print") && between.contains("pid ="),
        "nothing between `launchctl kill` and `launchctl bootout` waits for \
         manta to exit; bootout would SIGTERM a still-draining process:\n\
         {between}"
    );
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

/// launchd stops a job with `SIGTERM`, which manta does not handle, so the
/// only clean stop is to signal `SIGINT` and let it drain. Under an
/// unconditional `KeepAlive` launchd relaunches manta the instant that
/// drained process exits and the following `launchctl bootout` kills the
/// *replacement* abruptly, so the conditional `SuccessfulExit=false` form
/// is load-bearing for the documented stop sequence, not a style choice.
#[test]
fn launchd_keepalive_does_not_respawn_after_a_clean_drain() {
    let text = read(PLIST);
    let body: String = text
        .split("<key>KeepAlive</key>")
        .nth(1)
        .expect("plist must set KeepAlive")
        .split("</dict>")
        .next()
        .unwrap()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        body.starts_with("<dict><key>SuccessfulExit</key><false/>"),
        "KeepAlive must be the conditional dict with SuccessfulExit=false, \
         so a SIGINT drain (exit 0) is not immediately relaunched; found: \
         {body}"
    );
}

/// The path in `ProgramArguments[0]` is absolute and launchd searches no
/// `PATH`, so the documented macOS install has to actually create it. A
/// plist pointing at a binary the instructions never install bootstraps
/// without error and then fails on every spawn.
#[test]
fn documented_macos_install_creates_the_plist_program_path() {
    let plist = read(PLIST);
    let program = plist
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .expect("plist must set ProgramArguments")
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("<string>")?
                .strip_suffix("</string>")
                .map(str::to_string)
        })
        .expect("ProgramArguments must open with the executable path");

    let readme = read("packaging/README.md");
    let section = readme
        .split("## Installing on macOS")
        .nth(1)
        .expect("packaging/README.md must have a macOS install section");
    let section = section.split("\n## ").next().unwrap();
    assert!(
        section.lines().map(str::trim).any(|l| {
            l.starts_with("sudo install ")
                && l.split_whitespace().next_back() == Some(program.as_str())
        }),
        "the macOS instructions never install the binary at `{program}`, \
         the path the plist's ProgramArguments executes"
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

/// The lines of one block-mapping key's list in `docker-compose.yml`
/// (`ports:`, `volumes:`), trimmed of their `- ` and quotes.
fn compose_list(key: &str) -> Vec<String> {
    let text = read(COMPOSE);
    let mut out = Vec::new();
    let mut inside = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t == format!("{key}:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match t.strip_prefix("- ") {
            Some(item) => out.push(item.trim_matches('"').to_string()),
            // Any other non-comment line at this point has left the list.
            None => break,
        }
    }
    assert!(!out.is_empty(), "docker-compose.yml has no `{key}:` list");
    out
}

/// A bind mount without `z`/`Z` keeps its host SELinux label, which the
/// container's own domain cannot read: on an enforcing host (the
/// Fedora/RHEL default) manta cannot open its config at all and
/// `restart: unless-stopped` loops on it. The option is a no-op where
/// SELinux is not enforcing, so the shipped file can carry it always.
#[test]
fn compose_config_mount_carries_an_selinux_relabel_option() {
    let mount = compose_list("volumes")
        .into_iter()
        .find(|m| m.contains("/etc/manta/manta.toml"))
        .expect("docker-compose.yml must mount the config");
    let opts = mount.rsplit(':').next().unwrap();
    assert!(
        opts.split(',').any(|o| o == "z" || o == "Z"),
        "the config bind mount needs an SELinux relabel option (`Z`, or `z` \
         to share the label between containers): {mount}"
    );
}

/// The container side of every published port is manta's built-in default
/// -- nothing propagates a TOML override into the Compose file, so if the
/// defaults ever move, these mappings forward to ports nothing listens on.
#[test]
fn compose_publishes_the_config_default_ports() {
    let d = defaults();
    let mut expected = vec![d.telnet_port, d.json_port, d.metrics_port];
    expected.sort_unstable();

    let mut published: Vec<u16> = compose_list("ports")
        .iter()
        .map(|m| {
            m.rsplit(':')
                .next()
                .unwrap()
                .parse()
                .unwrap_or_else(|e| panic!("port mapping `{m}`: {e}"))
        })
        .collect();
    published.sort_unstable();
    assert_eq!(
        published, expected,
        "docker-compose.yml publishes container ports {published:?} but \
         manta's defaults are {expected:?}"
    );

    // The operator has to be told this coupling exists: a TOML port change
    // alone leaves the service unreachable from the host.
    let text = read(COMPOSE);
    assert!(
        text.contains("telnet_port") && text.contains("manta.toml"),
        "docker-compose.yml must document that a TOML port override needs \
         the matching container-side mapping changed too"
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

// -------------------------------------------------- shipped in the release

const RELEASE_WORKFLOW: &str = ".github/workflows/release-publish.yml";

/// The body of one `- name: <step>` block in the release workflow's build
/// job: everything up to the next step at the same indentation.
fn release_step(name: &str) -> String {
    let text = read(RELEASE_WORKFLOW);
    let marker = format!("      - name: {name}\n");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("{RELEASE_WORKFLOW} has no `{name}` step"))
        + marker.len();
    let rest = &text[start..];
    let end = rest.find("\n      - ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

/// packaging/README.md tells an operator who downloaded a release binary to
/// copy these files -- which only works if the archive actually contains
/// them. A packaging step that drops one turns every install command in
/// that file into a dangling reference to a checkout the operator was told
/// they would not need.
#[test]
fn release_archives_carry_every_unattended_asset() {
    for step in ["Package (Unix)", "Package (Windows)"] {
        let body = release_step(step);
        for asset in [EXAMPLE_TOML, COMPOSE] {
            assert!(
                body.contains(asset),
                "release step `{step}` never copies `{asset}` into the archive"
            );
        }
        // The units and packaging/README.md ship as the whole `packaging`
        // tree, so a new file added there needs no second edit in the
        // workflow -- assert the tree is copied, then that the files this
        // test knows about are in fact inside it.
        assert!(
            body.contains("packaging"),
            "release step `{step}` never copies the `packaging/` tree"
        );
    }
    for asset in [SERVICE, PLIST, LOGROTATE_PLIST, "packaging/README.md"] {
        assert!(
            repo_root().join(asset).is_file(),
            "`{asset}` is documented as shipped but is not in the tree"
        );
        assert!(
            asset.starts_with("packaging/"),
            "`{asset}` lives outside `packaging/`, so copying that tree does \
             not ship it -- the release workflow needs its own `cp` line"
        );
    }
}

/// Every in-repo path a shipped asset points the operator at has to be in
/// the archive too, at that same relative path. `manta.example.toml` tells
/// a no-clone release user to read the network-exposure runbook before
/// using the default `0.0.0.0` bind -- including for the unauthenticated
/// metrics port -- so an archive without it leaves safety-critical
/// guidance dangling at a path that does not exist.
#[test]
fn release_archives_carry_the_docs_the_shipped_assets_reference() {
    let mut referenced: Vec<String> = Vec::new();
    for asset in [EXAMPLE_TOML, COMPOSE] {
        let text = read(asset);
        let mut rest = text.as_str();
        while let Some(i) = rest.find("docs/") {
            let tail = &rest[i..];
            let end = tail
                .find(|c: char| c.is_whitespace() || "'\"`),;".contains(c))
                .unwrap_or(tail.len());
            let path = tail[..end].trim_end_matches('.');
            assert!(
                repo_root().join(path).is_file(),
                "`{asset}` references `{path}`, which is not a file in this \
                 repo"
            );
            if !referenced.iter().any(|p| p == path) {
                referenced.push(path.to_string());
            }
            rest = &tail[end..];
        }
    }
    assert!(
        referenced.iter().any(|p| p == EXPOSURE_RUNBOOK),
        "expected the shipped assets to point at `{EXPOSURE_RUNBOOK}`, \
         found {referenced:?}"
    );

    for step in ["Package (Unix)", "Package (Windows)"] {
        let body = release_step(step);
        for path in &referenced {
            assert!(
                body.contains(path.as_str()),
                "release step `{step}` never copies `{path}` into the \
                 archive, but a shipped asset tells the operator to read it \
                 at exactly that relative path"
            );
        }
    }
}
