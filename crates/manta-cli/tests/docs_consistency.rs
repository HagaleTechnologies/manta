//! The public docs must match the shipped binary. MAN-145.
//!
//! These assertions exist because README.md, ARCHITECTURE.md, CLAUDE.md and
//! wiki/pages/overview.md independently drifted to three different crate
//! counts and a two-milestone-stale description of the output layer. Each
//! test below pins one claim a first-time visitor reads.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// Every run of whitespace collapsed to a single space, so a phrase that a
/// Markdown reflow split across two lines still matches.
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

fn workspace_members() -> Vec<String> {
    let manifest: toml::Value = toml::from_str(&doc("Cargo.toml")).expect("parse Cargo.toml");
    manifest["workspace"]["members"]
        .as_array()
        .expect("members array")
        .iter()
        .map(|m| {
            m.as_str()
                .expect("member string")
                .rsplit('/')
                .next()
                .expect("crate name")
                .to_string()
        })
        .collect()
}

// ---- Scenario 3: the crate count is accurate everywhere ----

/// Every crate in the workspace must appear in ARCHITECTURE.md's tree.
/// `manta-soak-harness` was missing for the whole of M2/M3.
#[test]
fn architecture_lists_every_workspace_crate() {
    let arch = doc("ARCHITECTURE.md");
    for krate in workspace_members() {
        assert!(
            arch.contains(&krate),
            "ARCHITECTURE.md never mentions workspace member `{krate}`"
        );
    }
}

/// README, CLAUDE.md and the wiki overview must all state the same crate
/// count, and it must be the real one. ARCHITECTURE.md is deliberately not
/// in the list: it states no count in words, and is covered for crate
/// *names* by `architecture_lists_every_workspace_crate` above. Guard-
/// listing the stale spellings keeps the failure message actionable when a
/// crate is added.
#[test]
fn every_doc_states_the_real_crate_count() {
    let n = workspace_members().len();
    assert_eq!(
        n, 9,
        "crate count changed to {n}: update the docs below and this test"
    );

    let stale = [
        "seven-crate",
        "eight-crate",
        "8-crate",
        "7-crate",
        "six-crate",
    ];
    let fresh = ["nine-crate", "9-crate"];

    for file in ["README.md", "CLAUDE.md", "wiki/pages/overview.md"] {
        let text = doc(file);
        for s in stale {
            assert!(
                !text.contains(s),
                "{file} still says `{s}` (workspace has {n} crates)"
            );
        }
        assert!(
            fresh.iter().any(|f| text.contains(f)),
            "{file} states no crate count; expected one of {fresh:?}"
        );
    }
}

// ---- Scenario 2: the Inputs table lists every shipped input ----

/// Every input the CLI can be built with needs a row. HPSDR/Hermes shipped
/// (crates/manta-input/src/hpsdr.rs, feature `hpsdr`, which the README's
/// install line turns on) and had no row at all.
#[test]
fn readme_inputs_table_covers_every_shipped_input() {
    let readme = doc("README.md");
    let inputs = section(&readme, "Inputs");
    for needle in [
        "WAV file",
        "--device",
        "--kiwi-host",
        "--soapy-driver",
        "--hpsdr-host",
    ] {
        assert!(
            inputs.contains(needle),
            "README Inputs table has no row for `{needle}`"
        );
    }
}

// ---- Scenario 1: Outputs and Status describe shipped capability ----

/// The telnet server, JSON/WebSocket stream, RBN uplink and Prometheus
/// endpoint have all shipped and are covered by manta-server's test suite.
#[test]
fn readme_outputs_describes_shipped_servers() {
    let readme = doc("README.md");
    let outputs = section(&readme, "Outputs");
    assert!(
        !outputs.contains("in progress"),
        "README Outputs still calls a shipped server \"in progress\""
    );
    for needle in ["7300", "7301", "7302", "uplink", "--server-config"] {
        assert!(
            outputs.contains(needle),
            "README Outputs never mentions `{needle}`"
        );
    }
}

/// The Status block must not list shipped work as upcoming.
#[test]
fn readme_status_does_not_promise_shipped_work() {
    let readme = doc("README.md");
    let status = section(&readme, "Status");
    for stale in ["the telnet and JSON spot servers", "TOML config, metrics"] {
        assert!(
            !status.contains(stale),
            "README Status still lists `{stale}` as future work"
        );
    }
}

/// Every command in the Quickstart must run on a build the reader was told
/// to make. `--soapy-driver` needs `--features soapy`, which is in neither
/// the default build nor any release binary.
#[test]
fn readme_quickstart_only_uses_default_build_flags() {
    let readme = doc("README.md");
    let quickstart = section(&readme, "60-second demo");
    for gated in ["--soapy-driver", "--soapy-freq", "--soapy-rate"] {
        assert!(
            !quickstart.contains(gated),
            "60-second demo uses feature-gated flag `{gated}`; it fails with \
             `error: unexpected argument` on a default or release build"
        );
    }
}

// ---- Scenario 4: ROADMAP's RBN-admission item ----

/// RBN admission is active P0 work (MAN-40; decision D1, 2026-09-06), not a
/// deferred post-1.0 idea.
#[test]
fn roadmap_does_not_defer_rbn_admission() {
    let roadmap = doc("ROADMAP.md");
    let post = section(&roadmap, "Post-1.0 candidates");
    assert!(
        !post.contains("RBN operators"),
        "ROADMAP still defers RBN-operator admission to post-1.0"
    );
}

/// M3's own prose must not list the shipped manta-server as remaining.
#[test]
fn roadmap_m3_does_not_list_shipped_server_as_remaining() {
    let roadmap = doc("ROADMAP.md");
    // Collapse runs of whitespace so the search survives a paragraph reflow:
    // the phrase is line-wrapped in the source Markdown, and matching the raw
    // text would panic on `.expect()` instead of reporting a real drift.
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    assert!(
        !m3[idx..].contains("manta-server"),
        "ROADMAP M3 still lists `manta-server` as a remaining sub-project"
    );
}
