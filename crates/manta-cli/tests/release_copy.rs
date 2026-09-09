//! MAN-84 / decision D7 (2026-09-06 broad review): the properties that make
//! a published manta release honest, asserted in CI rather than trusted to a
//! reviewer's memory.
//!
//! These are text assertions over repo files, not over manta's behaviour, so
//! they read oddly next to the golden-vector tests around them. They live
//! here — in `manta-cli`, the crate whose binary is what a release actually
//! ships — because `cargo test --workspace` is what this repo's REQUIRED CI
//! checks run (`test (ubuntu-latest)` / `test (macos-latest)`, ci.yml). A new
//! standalone job would not be required and so would not gate a merge, and a
//! shell script in ci.yml's `test` job would have to stay bash-3.2-clean for
//! the macOS leg (the constraint MAN-65's own plan works under). A Rust test
//! sidesteps both.

use std::path::{Path, PathBuf};

/// Walk up from this crate to the workspace root. Anchored on two files that
/// must both exist there, so a stray `README.md` in an intermediate directory
/// can't produce a false root.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("README.md").is_file() && p.join(".github/workflows").is_dir())
        .expect("repo root not found from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn read_lowercased(rel: &str) -> String {
    read_repo_file(rel).to_lowercase()
}

/// `version = "..."` from `Cargo.toml`'s `[workspace.package]` table — the
/// single value `verify-version` checks a pushed tag against, and therefore
/// the only version any repo prose may claim is current.
fn workspace_version() -> String {
    let manifest = read_repo_file("Cargo.toml");
    let table = manifest
        .split_once("[workspace.package]")
        .expect("Cargo.toml has no [workspace.package] table")
        .1;
    let table = table.split_once("\n[").map_or(table, |(head, _)| head);
    table
        .lines()
        .find_map(|l| l.trim().strip_prefix("version"))
        .and_then(|rest| rest.trim_start().strip_prefix('='))
        .map(|rest| rest.trim().trim_matches('"').to_string())
        .expect("no `version =` key in [workspace.package]")
}

/// Every `vX.Y.Z` literal in `text`, returned without the leading `v`. A `v`
/// that continues a word (`rev0.1.0`) is not a version reference and is
/// skipped; a run of digits and dots that isn't exactly three numeric
/// components (`v1.2`, `v1.2.3.4`) is not a release version either.
fn version_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (i, _) in text.char_indices().filter(|&(_, c)| c == 'v') {
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let run: String = text[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let parts: Vec<&str> = run.split('.').collect();
        if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
            found.push(run);
        }
    }
    found
}

/// D7 pins this exact phrase. Compared case-insensitively: prose capitalises
/// it at the start of a sentence ("**Pre-stability alpha, expect breakage.**")
/// and a case-sensitive check false-fails on correct copy.
const D7_PHRASE: &str = "pre-stability alpha, expect breakage";

#[test]
fn readme_status_declares_pre_stability_alpha() {
    assert!(
        read_lowercased("README.md").contains(D7_PHRASE),
        "README.md must say {D7_PHRASE:?} (MAN-84 scenario 2, decision D7). \
         The Installation section promises downloadable binaries; the Status \
         section is where a reader learns how much to trust them."
    );
}

#[test]
fn release_notes_declare_pre_stability_alpha() {
    assert!(
        read_lowercased(".github/workflows/release-publish.yml").contains(D7_PHRASE),
        "the `release` job's action-gh-release `body:` must say {D7_PHRASE:?} \
         (MAN-84 scenario 2). `generate_release_notes: true` alone yields only \
         an auto-generated PR list, with no statement of stability at all."
    );
}

/// Extracts the top-level job block named `name` from a GitHub Actions
/// workflow file: the `name:` header line under `jobs:` (2-space indent)
/// plus every following line, up to but not including the next 2-space-
/// indented, non-comment line (the next job). Scoping assertions to a named
/// job's own block, rather than grepping the whole file, means a string
/// that happens to occur in a NEIGHBOURING job can't satisfy a guard meant
/// for this one.
fn job_block(wf: &str, name: &str) -> String {
    let header = format!("  {name}:");
    let start = wf
        .lines()
        .position(|l| l == header)
        .unwrap_or_else(|| panic!("job `{name}` not found in workflow (looked for {header:?})"));
    let mut block = String::new();
    for line in wf.lines().skip(start) {
        if !block.is_empty()
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && !line.trim_start().starts_with('#')
        {
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    block
}

/// The (single-line) `if:` condition within a job block, as extracted by
/// `job_block`.
fn if_condition(block: &str) -> &str {
    block
        .lines()
        .find(|l| l.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("no `if:` line found in job block:\n{block}"))
}

#[test]
fn github_release_is_not_gated_on_the_ghcr_push() {
    let wf = std::fs::read_to_string(repo_root().join(".github/workflows/release-publish.yml"))
        .expect("cannot read release-publish.yml");

    let release_block = job_block(&wf, "release");
    let release_if = if_condition(&release_block);
    assert!(
        release_if.contains("needs.build.result == 'success'"),
        "the `release` job's own `if:` line must gate on the build matrix \
         succeeding (MAN-84: the GitHub Release binaries are the \
         deliverable). Line was: {release_if:?}"
    );
    assert!(
        !release_if.contains("docker-publish"),
        "the `release` job's own `if:` line must not reference \
         `docker-publish`'s result -- GHCR is MAN-65 finding 4 / MAN-66 and \
         explicitly not a blocker. A gate on docker-publish's result would \
         silently take the whole GitHub Release down with a failed GHCR \
         push. Line was: {release_if:?}"
    );

    let verify_version_block = job_block(&wf, "verify-version");
    assert!(
        verify_version_block.contains(r#""$TAG_VERSION" != "$CARGO_VERSION""#),
        "release-publish.yml must keep the `verify-version` job's actual \
         tag-vs-[workspace.package] comparison, not just the job's name or \
         an emptied-out script: a tag whose version disagrees with the \
         workspace would otherwise publish a Release whose binaries answer \
         a different version to --version. Job block was:\n{verify_version_block}"
    );
}

/// MAN-84 codex review: `README.md` is copied into every release archive, so
/// a hard-coded "vX.Y.Z is current" sentence in it goes stale the moment the
/// workspace is bumped — and the runbook only tells the operator to bump
/// `Cargo.toml`/`Cargo.lock`. Prose that names no version at all passes (the
/// release badge already renders the current one); prose that does name one
/// must name the workspace's own version. Scoped to README because
/// `docs/RUNBOOKS/release.md` deliberately uses `v0.1.0` as a worked example
/// of the commands, not as a claim about what is current.
#[test]
fn readme_names_no_stale_release_version() {
    let workspace = workspace_version();
    let stale: Vec<String> = version_literals(&read_repo_file("README.md"))
        .into_iter()
        .filter(|v| *v != workspace)
        .collect();
    assert!(
        stale.is_empty(),
        "README.md refers to release version(s) {stale:?} but \
         [workspace.package] version is {workspace:?}. Either drop the \
         version from the prose (the release badge shows the current one) or \
         update it in the same change that bumps the workspace."
    );
}
