//! MAN-549: the integration tests share one `MANTA_*` scrub and one port
//! reservation (`common/launch.rs`), and the golden tests share one
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
const PENDING_LAUNCHER_COPIES: [&str; 18] = [
    "build_identity.rs",
    "cli.rs",
    "cli_json_consumers.rs",
    "completions_and_man_pages.rs",
    "config_command.rs",
    "decode_observations.rs",
    "decode_sidecar.rs",
    "ftx_acceptance.rs",
    "grade_cmd.rs",
    "hpsdr_discovery.rs",
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

#[test]
fn only_common_launch_and_doctor_checks_define_free_ports() {
    // doctor_checks.rs keeps its private port band (MAN-333, MAN-410).
    let found = offenders(
        &["fn free_ports"],
        &["common/launch.rs", "doctor_checks.rs"],
    );
    assert!(
        found.is_empty(),
        "use common/launch.rs's `free_ports()`: {found:?}"
    );
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
const RELEASED_EPHEMERAL_PORT: [&str; 4] = [
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
const PENDING_RELEASED_PORTS: [&str; 5] = [
    "cli.rs",
    "common/ka9q.rs",
    "docker_image.rs",
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
    let held = [
        // A listener kept alive by the caller.
        "let listener = TcpListener::bind(\"127.0.0.1:0\").unwrap();\n\
         let port = listener.local_addr().unwrap().port();\n",
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
