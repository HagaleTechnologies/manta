//! MAN-549: the integration tests share one `MANTA_*` scrub and one `manta`
//! launcher (`common/launch.rs`), and the golden tests share one
//! `word_boundary_fraction` (`manta_testkit::cer`). A new test copies
//! whichever variant it finds, so these source scans keep one to find.

use std::path::{Path, PathBuf};

/// This file names the patterns it bans, so it never scans itself.
const SELF: &str = "shared_test_helpers.rs";

/// Every `.rs` file under `tests/`, as (path relative to `tests/` with `/`
/// separators, text), sorted.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(root).unwrap();
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if rel != SELF {
                    out.push((rel, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
    }
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

/// Roots that still carry their own byte-identical scrubbing `fn manta`.
/// MAN-549 shipped in two slices to stay within the pull-request size
/// limit; the follow-up deletes each copy (include `common/launch.rs` by
/// `#[path]` and `use launch::manta;`) and empties this list. Never add to
/// it.
const PENDING_LAUNCHER_COPIES: [&str; 17] = [
    "build_identity.rs",
    "cli.rs",
    "cli_json_consumers.rs",
    "completions_and_man_pages.rs",
    "config_command.rs",
    "decode_observations.rs",
    "decode_sidecar.rs",
    "ftx_acceptance.rs",
    "grade_cmd.rs",
    "iq_correction.rs",
    "live_audio_errors.rs",
    "live_state_acceptance.rs",
    "operator_guides.rs",
    "oracle_funnel.rs",
    "packaging_examples.rs",
    "replay_acceptance.rs",
    "source_check.rs",
];

/// Files containing any of `needles`, other than `allowed`.
fn offenders(needles: &[&str], allowed: &[&str]) -> Vec<String> {
    sources()
        .into_iter()
        .filter(|(rel, text)| {
            !allowed.contains(&rel.as_str()) && needles.iter().any(|n| text.contains(n))
        })
        .map(|(rel, _)| rel)
        .collect()
}

#[test]
fn only_common_launch_scrubs_manta_variables() {
    let mut allowed = vec!["common/launch.rs"];
    allowed.extend(PENDING_LAUNCHER_COPIES);
    let found = offenders(
        &["starts_with(b\"MANTA_\")", "starts_with(\"MANTA_\")"],
        &allowed,
    );
    assert!(
        found.is_empty(),
        "scrub inherited MANTA_* variables with common/launch.rs's `manta()` or \
         `without_manta_env()`, not a copy of its loop: {found:?}"
    );
}

/// MAN-569: whether a source line uses the released-port helper, however
/// it is qualified, imported, aliased or re-exported. The line is matched
/// on the exact ASCII identifier token `free_ports`: it is split on
/// every character that is not an ASCII alphanumeric or `_`, so qualifiers
/// (`launch::`), turbofish (`::<3>()`), braces and aliases need no special
/// handling, while `free_port` (the claimed band, singular) and
/// `find_free_ports` are different tokens and do not match. A standalone
/// `//` comment line names the helper only in prose.
fn uses_free_ports_token(line: &str) -> bool {
    if line.trim_start().starts_with("//") {
        return false;
    }
    line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|token| token == "free_ports")
}

/// `file:line` for every scanned line that uses or defines the `free_ports`
/// token. Only `doctor_checks.rs` keeps one, its private band allocator
/// (MAN-333, MAN-410). MAN-588 deleted the shared released-port definition
/// from `common/launch.rs` and its `common/mod.rs` re-export, so a definition,
/// re-export or call anywhere else brings a released number back.
fn free_ports_hits(sources: &[(String, String)]) -> Vec<String> {
    const DEFINITION_SITES: [&str; 1] = ["doctor_checks.rs"];
    let mut hits = Vec::new();
    for (rel, text) in sources {
        if DEFINITION_SITES.contains(&rel.as_str()) {
            continue;
        }
        for (index, line) in text.lines().enumerate() {
            if uses_free_ports_token(line) {
                hits.push(format!("{rel}:{}", index + 1));
            }
        }
    }
    hits
}

#[test]
fn no_integration_test_uses_released_free_ports() {
    let hits = free_ports_hits(&sources());
    assert!(
        hits.is_empty(),
        "the `free_ports()` helper releases its ports before a daemon binds \
         them, and a sibling socket can take the number in between: write \
         `telnet_port = 0`, `json_port = 0` and `metrics_port = 0` and read the \
         bound ports from the daemon's `listening:` banner \
         (`spawn_daemon_reporting_ports`), or take a concrete port from the \
         claimed band (`src/test_ports.rs`); do not define a new `free_ports`: {hits:?}"
    );
}

/// MAN-569 AC2's red path, pinned in-tree: an incident fixture that restores
/// the old released-port call is named `file:line` by the scan even though no
/// definition of the helper exists anywhere in the scanned sources. The
/// scratch mutation the decision record describes was a whole file under
/// `tests/` deleted after its red run; this keeps that proof in the tree.
#[test]
fn the_scan_names_a_fixture_that_restores_the_old_call() {
    let sources = [(
        "zzz_restored_incident_fixture.rs".to_string(),
        "let ports = launch::free_ports();\nlet metrics = ports[2];\n".to_string(),
    )];
    assert_eq!(
        free_ports_hits(&sources),
        ["zzz_restored_incident_fixture.rs:1".to_string()]
    );
}

/// MAN-588's red path: a definition or re-export restored in the files that
/// used to hold the shared helper, or a new definition in another root, is
/// named `file:line`; `doctor_checks.rs`'s private allocator is not.
#[test]
fn the_scan_names_a_restored_definition_or_re_export() {
    let sources = [
        (
            "common/launch.rs".to_string(),
            "pub fn free_ports<const N: usize>() -> [u16; N] {\n".to_string(),
        ),
        (
            "common/mod.rs".to_string(),
            "pub use launch::{free_ports, manta, without_manta_env};\n".to_string(),
        ),
        (
            "zzz_new_port_helper.rs".to_string(),
            "fn free_ports() -> [u16; 3] {\n".to_string(),
        ),
        (
            "doctor_checks.rs".to_string(),
            "fn free_ports<const N: usize>() -> [u16; N] {\n".to_string(),
        ),
    ];
    assert_eq!(
        free_ports_hits(&sources),
        [
            "common/launch.rs:1".to_string(),
            "common/mod.rs:1".to_string(),
            "zzz_new_port_helper.rs:1".to_string(),
        ]
    );
}

#[test]
fn released_free_ports_guard_matches_imports_and_calls() {
    let caught = [
        "let ports = launch::free_ports();",
        "let trio = common::free_ports::<3>();",
        "let ports = free_ports();",
        "use common::{free_ports, manta};",
        "use common::free_ports as pick_ports;",
        "pub use common::free_ports;",
    ];
    for line in caught {
        assert!(uses_free_ports_token(line), "missed: {line}");
    }
    let allowed = [
        "// let ports = free_ports();",
        "/// `free_ports` is gone; read the banner instead.",
        "let port = test_ports::free_port();",
        "let ports = find_free_ports();",
        "let note = free_ports_note;",
    ];
    for line in allowed {
        assert!(!uses_free_ports_token(line), "flagged: {line}");
    }
}

/// MAN-588: whether a source line declares the `decode` or `pileup` module.
/// Matched on adjacent identifier tokens, so `pub mod decode;`,
/// `pub(crate) mod pileup;` and `mod decode;` match and `mod decoder;` or a
/// `//` comment does not.
fn declares_golden_leaf(line: &str) -> bool {
    if line.trim_start().starts_with("//") {
        return false;
    }
    let tokens: Vec<&str> = line
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    tokens
        .windows(2)
        .any(|w| w[0] == "mod" && (w[1] == "decode" || w[1] == "pileup"))
}

/// MAN-588: `common/decode.rs` and `common/pileup.rs` bind no socket, and
/// neither do the golden roots that use them. Reached through `mod common;`,
/// whose closure binds (`fake_server`, `ka9q`), they made those roots
/// socket-repeat candidates (MAN-523 D3), so any edit under `tests/common/`
/// repeated every golden scene ten times and timed shards out (#606). The
/// roots include the two leaves by `#[path]`, and `common/mod.rs` declares
/// neither.
#[test]
fn common_mod_leaves_the_golden_decode_helpers_to_path_includes() {
    let hits: Vec<String> = sources()
        .into_iter()
        .filter(|(rel, _)| rel == "common/mod.rs")
        .flat_map(|(rel, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| declares_golden_leaf(line))
                .map(|(index, _)| format!("{rel}:{}", index + 1))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        hits.is_empty(),
        "include `common/decode.rs` and `common/pileup.rs` by \
         `#[path = \"common/decode.rs\"] mod decode;` in the root that needs them, \
         not through `mod common;`: a root that binds no socket then stays out of \
         CI's socket-repeat set: {hits:?}"
    );
}

#[test]
fn golden_leaf_matcher_matches_module_declarations_only() {
    for line in ["pub mod decode;", "mod pileup;", "pub(crate) mod decode;"] {
        assert!(declares_golden_leaf(line), "missed: {line}");
    }
    for line in [
        "// pub mod decode;",
        "/// `pub mod pileup;` is gone.",
        "mod decoder;",
        "mod ipfix;",
        "use decode::per_track;",
    ] {
        assert!(!declares_golden_leaf(line), "flagged: {line}");
    }
}

#[test]
fn every_local_manta_launcher_wraps_the_shared_one() {
    // These add one step (stdin, RUST_LOG, args) around `launch::manta()`
    // or `common::manta()`; any other `fn manta` is a copy.
    let mut allowed = vec![
        "common/launch.rs",
        "bench_sensitivity.rs",
        "calibrate.rs",
        "man203_conditioning.rs",
        "man234_bad_samples.rs",
        "output_style.rs",
    ];
    allowed.extend(PENDING_LAUNCHER_COPIES);
    let found = offenders(&["fn manta("], &allowed);
    assert!(
        found.is_empty(),
        "use common/launch.rs's `manta()`: {found:?}"
    );
}

#[test]
fn word_boundary_fraction_lives_in_manta_testkit() {
    let found = offenders(
        &["fn word_boundary_fraction(", "fn char_alignment_ops("],
        &[],
    );
    assert!(
        found.is_empty(),
        "call `manta_testkit::cer::word_boundary_fraction`: {found:?}"
    );
}

/// The pending list only shrinks: an entry whose file no longer carries a
/// copy (its own `fn manta` and scrub loop) must leave the list, so a
/// converted file loses its exemption from both rules above.
#[test]
fn pending_launcher_copies_are_still_copies() {
    let sources = sources();
    let stale: Vec<&str> = PENDING_LAUNCHER_COPIES
        .iter()
        .copied()
        .filter(|name| {
            !sources.iter().any(|(rel, text)| {
                rel == name
                    && text.contains("fn manta(")
                    && text.contains("starts_with(b\"MANTA_\")")
            })
        })
        .collect();
    assert!(
        stale.is_empty(),
        "remove from PENDING_LAUNCHER_COPIES, they no longer carry their own scrubbing \
         `fn manta`: {stale:?}"
    );
}

/// MAN-523 (after MAN-410 and MAN-508): the shapes that bind an ephemeral
/// port, release it and hand its number on for later use. A sibling test
/// can bind the freed number first. Compared with all whitespace removed.
const RELEASED_EPHEMERAL_PORT: [&str; 6] = [
    // The metrics listener is released before a configured daemon binds it.
    "letmetrics=held.local_addr().unwrap().port();let(_file,mutcommand)=configured_command(radio.port,flags,mac,metrics);drop(held);",
    // Three listeners collected into an array and then released.
    "letports=held.each_ref().map(|listener|listener.local_addr().unwrap().port());drop(held);",
    // The listener is a temporary, dropped at the end of the statement.
    "TcpListener::bind(\"127.0.0.1:0\").unwrap().local_addr()",
    "UdpSocket::bind(\"127.0.0.1:0\").unwrap().local_addr()",
    // A block that binds and yields only the port.
    "letlistener=TcpListener::bind(\"127.0.0.1:0\").unwrap();listener.local_addr().unwrap().port()}",
    // The address is kept and the listener dropped.
    "letaddr=listener.local_addr().unwrap();drop(listener);",
];

/// Sources that still release an ephemeral port: test-only UDP and TCP
/// ports whose collision MAN-523 has not seen. Never add to it; a file
/// that stops matching must leave it.
const PENDING_RELEASED_PORTS: [&str; 4] = [
    "cli.rs",
    "common/ka9q.rs",
    "multi_input_ka9q.rs",
    "n1mm_spectrum.rs",
];

/// Inline unit-test modules under `src/` that MAN-523 moved to the claimed
/// band in `src/test_ports.rs`, scanned with the integration tests.
const UNIT_SOURCES: [&str; 2] = ["list_http.rs", "list_refresh.rs"];

fn releases_an_ephemeral_port(text: &str) -> bool {
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    RELEASED_EPHEMERAL_PORT
        .iter()
        .any(|shape| squeezed.contains(shape))
}

/// Integration-test sources, then the named unit-test sources as `src/…`.
fn port_sources() -> Vec<(String, String)> {
    let mut out = sources();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for name in UNIT_SOURCES {
        let text = std::fs::read_to_string(src.join(name)).unwrap();
        out.push((format!("src/{name}"), text));
    }
    out
}

#[test]
fn the_released_port_shapes_match_the_copies_man523_removed() {
    let removed = [
        // health_command.rs's `free_port()`.
        "fn free_port() -> u16 {\n    TcpListener::bind(\"127.0.0.1:0\")\n        .unwrap()\n        \
         .local_addr()\n        .unwrap()\n        .port()\n}\n",
        // docker_image.rs's temporary metrics listener, whose number the
        // mounted TOML and the health command shared.
        "let metrics_port = std::net::TcpListener::bind(\"127.0.0.1:0\")\n    .unwrap()\n    \
         .local_addr()\n    .unwrap()\n    .port();\n",
        // list_http.rs's `a_closed_port_is_connection_refused`.
        "let port = {\n    let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n    \
         listener.local_addr().unwrap().port()\n};\n",
        // list_refresh.rs's `closed_url`.
        "let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n\
         let addr = listener.local_addr().unwrap();\ndrop(listener);\n",
    ];
    for copy in removed {
        assert!(releases_an_ephemeral_port(copy), "missed:\n{copy}");
    }
    let extra = [
        "let metrics = held.local_addr().unwrap().port(); let (_file, mut command) = configured_command(radio.port, flags, mac, metrics); drop(held);",
        "let ports = held.each_ref().map(|listener| listener.local_addr().unwrap().port()); drop(held);",
    ];
    for shape in extra {
        assert!(releases_an_ephemeral_port(shape), "missed:\n{shape}");
    }
    let held = [
        // A listener kept alive by the caller.
        "let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n\
         let port = listener.local_addr().unwrap().port();\n",
        "let metrics = held.local_addr().unwrap().port(); let _live = &held;",
        // The claimed band.
        "let port = crate::test_ports::free_port();\n",
    ];
    for shape in held {
        assert!(!releases_an_ephemeral_port(shape), "flagged:\n{shape}");
    }
}

#[test]
fn no_test_releases_an_ephemeral_port_for_reuse() {
    let found: Vec<String> = port_sources()
        .into_iter()
        .filter(|(rel, text)| {
            !PENDING_RELEASED_PORTS.contains(&rel.as_str()) && releases_an_ephemeral_port(text)
        })
        .map(|(rel, _)| rel)
        .collect();
    assert!(
        found.is_empty(),
        "hold the socket until the probe ends, or take a port from the claimed band \
         (src/test_ports.rs): {found:?}"
    );
}

#[test]
fn pending_released_ports_still_release_one() {
    let sources = port_sources();
    let stale: Vec<&str> = PENDING_RELEASED_PORTS
        .iter()
        .copied()
        .filter(|name| {
            !sources
                .iter()
                .any(|(rel, text)| rel == name && releases_an_ephemeral_port(text))
        })
        .collect();
    assert!(
        stale.is_empty(),
        "remove from PENDING_RELEASED_PORTS, they no longer release a port: {stale:?}"
    );
}

#[test]
fn only_man570_daemon_defines_spawn_daemon_reporting_ports() {
    let found = offenders(&["fn spawn_daemon_reporting_ports"], &["man570/daemon.rs"]);
    assert!(
        found.is_empty(),
        "use man570/daemon.rs's daemon launcher: {found:?}"
    );
}
