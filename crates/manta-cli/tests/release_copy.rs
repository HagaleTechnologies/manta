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

fn read_lowercased(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .to_lowercase()
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

#[test]
fn github_release_is_not_gated_on_the_ghcr_push() {
    let wf = std::fs::read_to_string(repo_root().join(".github/workflows/release-publish.yml"))
        .expect("cannot read release-publish.yml");
    assert!(
        wf.contains("needs.build.result == 'success'"),
        "the `release` job must gate on the build matrix succeeding, not on \
         `docker-publish` succeeding (MAN-84: the GitHub Release binaries are \
         the deliverable; GHCR is MAN-65 finding 4 / MAN-66 and explicitly not \
         a blocker). A plain `needs:` dependency takes the Release down with a \
         failed GHCR push."
    );
    assert!(
        wf.contains("verify-version"),
        "release-publish.yml must keep the `verify-version` job: a tag whose \
         version disagrees with [workspace.package] publishes a Release whose \
         binaries answer a different version to --version."
    );
}
