//! MAN-148: operator examples must load in the shipped CLI and match the service kit.

use regex::Regex;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GUIDES: &[&str] = &[
    "docs/GUIDES/rbn-node.md",
    "docs/GUIDES/kiwisdr.md",
    "docs/GUIDES/raspberry-pi.md",
];

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
        .replace("\r\n", "\n")
}

fn fenced_lines(md: &str) -> Vec<&str> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out
}

fn squash_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text between a `## <heading>` and the next `## ` heading.
fn section<'a>(md: &'a str, heading: &str) -> &'a str {
    let start = md
        .find(&format!("\n## {heading}"))
        .unwrap_or_else(|| panic!("no `## {heading}` section"));
    let rest = &md[start + 1..];
    let end = rest[3..].find("\n## ").map(|i| i + 4).unwrap_or(rest.len());
    &rest[..end]
}

fn toml_blocks(md: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut language = None;
    let mut block = String::new();
    for line in md.lines() {
        if let Some(fence) = line.trim().strip_prefix("```") {
            if let Some(lang) = language.take() {
                if lang == "toml" {
                    blocks.push(std::mem::take(&mut block));
                }
            } else {
                language = Some(fence);
            }
        } else if language == Some("toml") {
            block.push_str(line);
            block.push('\n');
        }
    }
    assert!(language.is_none(), "unclosed code fence");
    blocks
}

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn check_config(config: &toml::Table) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manta.toml");
    fs::write(&path, toml::to_string(config).unwrap()).unwrap();
    manta()
        .args(["config", "check", "--config"])
        .arg(path)
        .output()
        .expect("run manta config check")
}

/// Replace only string values, leaving comments and numeric ports out of the fixture.
fn replace_placeholders(value: &mut toml::Value) -> usize {
    match value {
        toml::Value::String(s) if s == "N0CALL" => {
            *s = "W5AU".into();
            1
        }
        toml::Value::String(s) if s.starts_with('<') && s.ends_with('>') => {
            *s = match s.as_str() {
                "<your-kiwi-host>" => "kiwi.example.net",
                "<collector-host>" => "collector.example.net",
                "<receiver-password>" => "secret",
                "<your-hpsdr-host>" => "192.0.2.10",
                other => panic!("add a fixed test value for {other}"),
            }
            .into();
            1
        }
        toml::Value::Table(t) => t.iter_mut().map(|(_, v)| replace_placeholders(v)).sum(),
        toml::Value::Array(a) => a.iter_mut().map(replace_placeholders).sum(),
        _ => 0,
    }
}

#[test]
fn node_guide_walks_the_ticket_steps_in_order() {
    let guide = doc(GUIDES[0]);
    let mut in_fence = false;
    let headings: Vec<String> = guide
        .lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                return false;
            }
            !in_fence && line.starts_with('#')
        })
        .map(str::to_lowercase)
        .collect();
    let mut remaining = headings.as_slice();
    for step in [
        "receiver", "config", "telnet", "uplink", "firewall", "service", "working",
    ] {
        let at = remaining
            .iter()
            .position(|h| h.contains(step))
            .unwrap_or_else(|| panic!("missing or out-of-order {step} heading: {headings:?}"));
        remaining = &remaining[at + 1..];
    }
}

#[test]
fn guide_configs_load() {
    let mut checked = 0;
    for guide in GUIDES {
        let blocks: Vec<toml::Table> = toml_blocks(&doc(guide))
            .iter()
            .map(|block| toml::from_str(block).unwrap_or_else(|e| panic!("{guide}: {e}")))
            .collect();
        for block in &blocks {
            let mut config = if block.contains_key("server") {
                block.clone()
            } else {
                let mut full = blocks
                    .iter()
                    .find(|b| b.contains_key("server"))
                    .unwrap_or_else(|| panic!("{guide}: snippet needs a full config"))
                    .clone();
                for (key, value) in block {
                    if key != "input" {
                        assert!(!full.contains_key(key), "{guide}: duplicate table {key}");
                    }
                    full.insert(key.clone(), value.clone());
                }
                full
            };
            let mut substituted = toml::Value::Table(config.clone());
            if replace_placeholders(&mut substituted) > 0 {
                let out = check_config(&config);
                assert_eq!(out.status.code(), Some(1), "{guide}: placeholders accepted");
            }
            config = substituted.as_table().unwrap().clone();
            let out = check_config(&config);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if config["input"]["type"].as_str() == Some("hpsdr") && !cfg!(feature = "hpsdr") {
                assert_eq!(out.status.code(), Some(1), "{guide}: {stdout}\n{stderr}");
                assert!(
                    stderr.contains("needs a manta built with --features hpsdr"),
                    "{guide}: {stderr}"
                );
            } else {
                assert_eq!(out.status.code(), Some(0), "{guide}: {stdout}\n{stderr}");
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no config examples checked");
}

#[test]
fn guide_uplinks_stay_in_dry_run() {
    for guide in GUIDES {
        for block in toml_blocks(&doc(guide)) {
            assert!(
                !block
                    .lines()
                    .any(|l| l.trim().starts_with("dry_run = false")),
                "{guide}: live uplink example"
            );
            let table: toml::Table = toml::from_str(&block).unwrap();
            if let Some(uplinks) = table.get("rbn_uplink") {
                for uplink in uplinks.as_array().expect("uplink array") {
                    assert_ne!(
                        uplink.get("dry_run").and_then(toml::Value::as_bool),
                        Some(false),
                        "{guide}: live uplink example"
                    );
                }
            }
        }
    }
}

#[test]
fn guide_relative_links_resolve() {
    let link = Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    for guide in GUIDES {
        let base = repo_root().join(guide).parent().unwrap().to_path_buf();
        let text = doc(guide);
        let mut checked = 0;
        for cap in link.captures_iter(&text) {
            let target = &cap[1];
            if target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("mailto:")
                || target.starts_with('#')
            {
                continue;
            }
            let path = target.split('#').next().unwrap();
            assert!(base.join(path).exists(), "{guide}: broken link {target}");
            checked += 1;
        }
        assert!(checked > 0, "{guide}: no relative links checked");
    }
}

#[test]
fn guide_commands_name_real_subcommands() {
    let mut commands = BTreeSet::new();
    for guide in GUIDES {
        let text = doc(guide);
        for line in fenced_lines(&text) {
            let line = line.trim().strip_prefix("$ ").unwrap_or(line.trim());
            let mut words = line.split_whitespace();
            let mut program = words.next();
            if program == Some("sudo") {
                program = words.next();
                if program == Some("-u") {
                    words.next();
                    program = words.next();
                }
            }
            if matches!(program, Some("manta" | "./manta" | "/usr/local/bin/manta")) {
                if let Some(sub) = words.next() {
                    if sub.bytes().all(|b| b.is_ascii_lowercase()) {
                        commands.insert(sub.to_string());
                    }
                }
            }
        }
    }
    assert!(!commands.is_empty(), "no guide commands checked");
    for sub in commands {
        let out = manta().args([&sub, "--help"]).output().unwrap();
        assert!(
            out.status.success(),
            "unknown subcommand {sub}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn guide_service_lines_match_the_kit() {
    let kit = doc("packaging/README.md");
    let unit = doc("packaging/systemd/manta.service");
    let config_path = unit
        .lines()
        .find_map(|line| line.strip_prefix("LoadCredential=manta.toml:"))
        .expect("kit loads a config credential");
    let mut checked = 0;
    for guide in GUIDES {
        let text = doc(guide);
        for line in fenced_lines(&text) {
            let line = line.trim();
            if line.starts_with("sudo install ")
                && (line.ends_with("/etc/systemd/system/manta.service")
                    || line.ends_with("/etc/manta/manta.toml"))
            {
                assert!(
                    kit.lines().any(|l| l == line),
                    "{guide}: service install differs from kit: {line}"
                );
                if line.ends_with("/etc/manta/manta.toml") {
                    assert_eq!(line.split_whitespace().last(), Some(config_path));
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 2, "service/config install lines missing");
}

#[test]
fn kiwi_commands_use_placeholders() {
    let mut checked = 0;
    for guide in GUIDES {
        let text = doc(guide);
        for line in fenced_lines(&text) {
            let words: Vec<_> = line.split_whitespace().collect();
            for (i, word) in words.iter().enumerate() {
                let host = if *word == "--kiwi-host" {
                    Some(*words.get(i + 1).expect("--kiwi-host needs a value"))
                } else {
                    word.strip_prefix("--kiwi-host=")
                };
                if let Some(host) = host {
                    assert!(
                        host.starts_with('<') && host.ends_with('>'),
                        "{guide}: unmarked receiver {host}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 0, "no Kiwi command checked");
}

#[test]
fn kiwi_guide_states_the_etiquette() {
    let text = squash_whitespace(&doc(GUIDES[1]).to_lowercase());
    for phrase in [
        "one channel",
        "not an rbn-grade node",
        "password",
        "time limit",
        "ask the owner",
        "-5000 to 5000 hz",
    ] {
        assert!(text.contains(phrase), "Kiwi guide omits {phrase}");
    }
}

#[test]
fn pi_guide_stays_within_d6() {
    let text = squash_whitespace(&doc(GUIDES[2]).to_lowercase());
    for phrase in [
        "pi 5",
        "pi 4",
        "has not been measured",
        "aarch64",
        "rustup",
        "get_throttled",
    ] {
        assert!(text.contains(phrase), "Pi guide omits {phrase}");
    }
    for target in [
        "2026-09-06-broad-review-decisions.md",
        "m2-pi4-cpu-budget.md",
    ] {
        assert!(
            text.contains(&format!("/{target}")),
            "Pi guide must link {target}"
        );
    }
}

#[test]
fn entry_points_link_the_guides() {
    let readme = doc("README.md");
    for guide in GUIDES {
        assert!(
            section(&readme, "Documentation").contains(&format!("]({guide})")),
            "README documentation must link {guide}"
        );
    }
    for (heading, guide) in [
        ("Run it as a node", GUIDES[0]),
        ("60-second demo", GUIDES[1]),
    ] {
        assert!(
            section(&readme, heading).contains(&format!("]({guide})")),
            "README {heading} must link {guide}"
        );
    }
    assert!(doc("packaging/README.md").contains("](../docs/GUIDES/kiwisdr.md)"));
}

/// MAN-90: the node guide names the Aggregator route before the uplink step
/// and calls the uplink experimental in that step.
#[test]
fn node_guide_leads_with_the_aggregator_route_and_marks_the_uplink_experimental() {
    let guide = doc(GUIDES[0]);
    let step5 = guide.find("\n## 5.").expect("no step 5");
    let route = guide
        .find("supported route into RBN")
        .expect("guide never names the supported route");
    assert!(
        route < step5,
        "the supported route appears only after step 5"
    );
    assert!(
        section(&guide, "5. Connect the uplink in dry-run").contains("experimental"),
        "step 5 never calls the uplink experimental"
    );
}
