//! The operator procedure stays linked and uses commands the shipped CLI accepts.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn doc(path: &str) -> String {
    fs::read_to_string(root().join(path)).unwrap()
}
const RUNBOOK: &str = "docs/RUNBOOKS/aggregator-swap.md";

#[test]
fn the_swap_runbook_exists_with_its_sections_and_links() {
    let text = doc(RUNBOOK);
    for heading in [
        "What this proves",
        "Before you start",
        "Stage A. Bench pre-flight",
        "Stage B. Live swap",
        "Stage C. Evidence",
        "Incompatibilities found",
        "Runs",
    ] {
        assert!(text.contains(&format!("## {heading}")), "missing {heading}");
    }
    assert!(text.contains("| Symptom | Where recorded | Disposition: fixed or known limitation |"));
    assert!(text.contains("## Runs\n\n(no live runs yet)"));
    assert!(text.contains("../DECISIONS/2026-11-19-man17-aggregator-swap-runbook.md"));
}

#[test]
fn the_live_swap_requires_source_attributed_evidence_before_and_after_cutover() {
    let text = doc(RUNBOOK);
    let (_, live) = text.split_once("## Stage B. Live swap").unwrap();
    let (live, evidence) = live.split_once("## Stage C. Evidence").unwrap();
    let (evidence, _) = evidence.split_once("## Incompatibilities found").unwrap();

    let (startup, steps) = live.split_once("\n1. ").unwrap();
    let (secondary, cutover) = steps.split_once("\n3. ").unwrap();
    assert!(
        startup.contains("manta run --config"),
        "missing live startup"
    );
    assert!(startup.contains("live SDR"), "missing live input");
    assert!(secondary.contains("secondary source number"));
    for field in ["spotted callsign", "frequency", "UTC"] {
        assert!(secondary.contains(field), "missing match field: {field}");
    }
    for link in [
        "https://reversebeacon.net/pages/Telnet%2Bservers%2B30",
        "https://reversebeacon.net/raw_data/",
    ] {
        assert!(
            secondary.contains(link),
            "missing RBN evidence link: {link}"
        );
    }
    assert!(secondary.contains("unanswered SETT"));
    assert!(!secondary.contains("five minutes"));
    assert!(cutover.contains("Primary Skimmer Connection"));
    assert!(cutover.contains("source `0`"));
    assert!(cutover.contains("after cutover"));
    assert!(cutover.contains("restore the saved primary settings"));
    assert!(evidence.contains("rollback"));
}

#[test]
fn every_manta_command_in_the_runbook_runs_or_matches_help() {
    let text = doc(RUNBOOK);
    let mut checked = 0;
    let mut in_shell = false;
    for line in text.lines() {
        if line == "```sh" {
            in_shell = true;
            continue;
        }
        if line.starts_with("```") {
            in_shell = false;
            continue;
        }
        if !in_shell || !line.starts_with("manta ") {
            continue;
        }
        let parts: Vec<_> = line.split_whitespace().collect();
        let subcommands = parts
            .iter()
            .copied()
            .skip(1)
            .take_while(|arg| !arg.starts_with('-'));
        let output = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(subcommands)
            .arg("--help")
            .output()
            .unwrap();
        assert!(output.status.success(), "{line}");
        let help = String::from_utf8_lossy(&output.stdout);
        for flag in parts.iter().filter(|s| s.starts_with("--")) {
            assert!(
                help.contains(flag),
                "{flag} absent from command help: {line}"
            );
        }
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn the_guides_that_name_the_aggregator_route_link_the_runbook() {
    assert!(doc("docs/GUIDES/rbn-node.md").contains("../RUNBOOKS/aggregator-swap.md"));
    assert!(doc("docs/RUNBOOKS/secondary-skimmer-field-node.md").contains("aggregator-swap.md"));
    let text = doc(RUNBOOK);
    let dir = root().join("docs/RUNBOOKS");
    for chunk in text.split("](").skip(1) {
        let target = chunk.split(')').next().unwrap().split('#').next().unwrap();
        if !target.starts_with("http") {
            assert!(dir.join(target).exists(), "broken link: {target}");
        }
    }
}
