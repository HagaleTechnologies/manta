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
    let found = offenders(
        &["starts_with(b\"MANTA_\")", "starts_with(\"MANTA_\")"],
        &["common/launch.rs"],
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
    let wrappers = [
        "common/launch.rs",
        "bench_sensitivity.rs",
        "calibrate.rs",
        "man203_conditioning.rs",
        "man234_bad_samples.rs",
        "output_style.rs",
    ];
    let found = offenders(&["fn manta("], &wrappers);
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
