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

/// Every crate in the workspace must be named somewhere in ARCHITECTURE.md
/// (the assertion searches the whole document, not only the crate tree --
/// a crate dropped from the tree but still named in prose passes here, and
/// that is deliberate: prose mentions are a legitimate home for a crate the
/// tree does not draw). `manta-soak-harness` was named nowhere at all for
/// the whole of M2/M3.
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
    let inputs = squash_whitespace(section(&readme, "Inputs"));
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
    // Squashed like the ROADMAP guard below: a reflow that split one of
    // these phrases across two lines would otherwise disable the assertion
    // silently instead of reporting real drift.
    let outputs = squash_whitespace(section(&readme, "Outputs"));
    assert!(
        !outputs.contains("in progress"),
        "README Outputs still calls a shipped server \"in progress\""
    );
    for needle in ["7300", "7301", "7302", "uplink", "--config"] {
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
    let status = squash_whitespace(section(&readme, "Status"));
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
    let quickstart = squash_whitespace(section(&readme, "60-second demo"));
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
    let post = squash_whitespace(section(&roadmap, "Post-1.0 candidates"));
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

// ---- Runnable-demo hygiene: no unmarked placeholder receivers ----

/// Lines inside ```-fenced blocks, so prose and table cells (which name
/// `--kiwi-host` without an argument) are not mistaken for commands.
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

/// A command a newcomer is invited to copy must not carry a hostname that
/// can never resolve to a receiver. The demo shipped `--kiwi-host
/// kiwi.example.org` -- reserved by RFC 2606, so the advertised
/// hardware-free live path dead-ends on a connection error with nothing in
/// the README saying the host was yours to supply. Every `--kiwi-host`
/// argument in a copyable block must be an angle-bracket placeholder, and
/// the demo must point at somewhere real receivers are listed.
#[test]
fn readme_kiwi_commands_use_a_marked_placeholder_host() {
    let readme = doc("README.md");
    for line in fenced_lines(&readme) {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok != "--kiwi-host" {
                continue;
            }
            let host = toks
                .next()
                .unwrap_or_else(|| panic!("`--kiwi-host` with no argument in: {line}"));
            assert!(
                host.starts_with('<') && host.ends_with('>'),
                "README command uses `--kiwi-host {host}`; a copyable command needs an \
                 explicit `<placeholder>` the reader must replace, not a host that \
                 cannot connect"
            );
        }
    }

    let demo = squash_whitespace(section(&readme, "60-second demo"));
    assert!(
        demo.contains("kiwisdr.com/public") || demo.contains("rx.linkfanel.net"),
        "60-second demo names a KiwiSDR placeholder but points nowhere the reader \
         can find a real public receiver"
    );
}

// ---- Review round 1 (PR #142): the documented commands must stay coherent ----

/// Fenced lines with shell line-continuations joined, so a command split
/// across two lines with a trailing `\` is still inspected as one command.
fn fenced_commands(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in fenced_lines(md) {
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        out.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

/// Every documented way of building the CLI must produce the same binary the
/// rest of the README describes. `manta-cli` declares no default features and
/// gates `--hpsdr-host` behind `hpsdr`, so a build command without
/// `--features hpsdr` yields a binary that rejects the input flag the
/// Installation notes and the Inputs table both advertise.
#[test]
fn readme_build_commands_keep_the_hpsdr_feature() {
    let readme = doc("README.md");
    for line in readme.lines() {
        if !line.contains("manta-cli") {
            continue;
        }
        if !(line.contains("cargo build") || line.contains("cargo install")) {
            continue;
        }
        assert!(
            line.contains("--features hpsdr"),
            "README builds manta-cli without `--features hpsdr`, so the binary has no \
             `--hpsdr-host` flag: {line}"
        );
    }
}

/// The Kiwi prose tells the reader to substitute the receiver's hostname
/// *and port*, and `--kiwi-port` defaults to 8073 in silence -- so a command
/// that offers only a host placeholder cannot reach a receiver on any other
/// port. Both copyable Kiwi commands need a marked port placeholder too.
#[test]
fn readme_kiwi_commands_offer_a_marked_placeholder_port() {
    let readme = doc("README.md");
    for cmd in fenced_commands(&readme) {
        if !cmd.contains("--kiwi-host") {
            continue;
        }
        let mut toks = cmd.split_whitespace();
        let mut port = None;
        while let Some(tok) = toks.next() {
            if tok == "--kiwi-port" {
                port = toks.next();
            }
        }
        let port = port.unwrap_or_else(|| {
            panic!(
                "README Kiwi command has no `--kiwi-port`, so it silently uses the 8073 \
                 default the surrounding prose tells the reader to replace: {cmd}"
            )
        });
        assert!(
            port.starts_with('<') && port.ends_with('>'),
            "README command uses `--kiwi-port {port}`; the reader must be shown a \
             `<placeholder>` to replace, not a fixed port"
        );
    }
}

/// M3's remaining-work prose must not quote a subset of the accept-when list.
/// It previously named the 7-day soak but not the stock-DX-cluster-client
/// session, which this branch's own verification notes leave open -- making
/// M3 read as closer to acceptance than it is.
#[test]
fn roadmap_m3_remaining_work_names_every_open_acceptance_gate() {
    let roadmap = doc("ROADMAP.md");
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    let remaining = &m3[idx..];
    for gate in [
        "parity benchmark",
        "7-day unattended soak",
        "cqdx",
        "stock DX-cluster client",
    ] {
        assert!(
            remaining.contains(gate),
            "ROADMAP M3's remaining-work prose never mentions the open accept-when gate \
             `{gate}`"
        );
    }
}
