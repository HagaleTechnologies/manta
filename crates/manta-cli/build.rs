//! MAN-128: embeds the git commit into `manta_build_info`. Never fails the
//! build: a Docker context (`.dockerignore` excludes `.git`) or a source
//! tarball has no git metadata and reports "unknown". `MANTA_GIT_SHA`
//! overrides (for release images that pass it as a build-arg).
//!
//! MAN-83: the same commit also feeds `manta --version` and the JSON
//! stream's `decoderVersion` (`manta-<version>+<sha>`), so the override must
//! be SemVer build metadata (dot-separated `[0-9A-Za-z-]` identifiers, at
//! most 64 characters); anything else is warned about and ignored. An
//! all-hex override longer than 12 characters (CI's 40-hex `github.sha`) is
//! cut to 12 and lowercased, so it matches a native build of that commit.
//! This script also exports `MANTA_FEATURES`, the compiled-in Cargo feature
//! list. That one is compile-time only: it is derived from Cargo's
//! `CARGO_FEATURE_*` variables, never read from the environment, so it is
//! not on `config.rs`'s `ENV_IGNORED` list.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// SemVer 2.0.0 build metadata: dot-separated, non-empty identifiers of
/// `[0-9A-Za-z-]`. The 64-character cap keeps `--version` one short line.
fn is_build_metadata(s: &str) -> bool {
    s.len() <= 64
        && s.split('.')
            .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// The `MANTA_GIT_SHA` override, validated (MAN-83 D5). `None` when unset,
/// empty, or not build metadata -- the last with a `cargo:warning`, and the
/// caller then falls through to `git` exactly as if it were unset.
fn sha_override() -> Option<String> {
    let s = std::env::var("MANTA_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())?;
    if !is_build_metadata(&s) {
        println!(
            "cargo:warning=MANTA_GIT_SHA={s:?} is not SemVer build metadata \
             (dot-separated [0-9A-Za-z-] identifiers, at most 64 characters); ignoring it"
        );
        return None;
    }
    if s.len() > 12 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(s[..12].to_ascii_lowercase());
    }
    Some(s)
}

fn main() {
    println!("cargo:rerun-if-env-changed=MANTA_GIT_SHA");
    let sha = sha_override()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());

    // Rebuild when HEAD moves -- only for paths that exist (a missing
    // rerun-if-changed path makes Cargo rerun this script on every build).
    // The paths `git rev-parse` returns are relative to this script's cwd
    // (the crate directory), so join against CARGO_MANIFEST_DIR.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    for (flag, rel) in [("--git-dir", "HEAD"), ("--git-common-dir", "packed-refs")] {
        if let Some(dir) = git(&["rev-parse", flag]) {
            let p = std::path::Path::new(&manifest_dir).join(&dir).join(rel);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(dir) = git(&["rev-parse", "--git-common-dir"]) {
            let p = std::path::Path::new(&manifest_dir).join(&dir).join(&r);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
    println!("cargo:rustc-env=MANTA_GIT_SHA={sha}");

    // MAN-83: compiled-in optional features, for `--version` and
    // `manta_build_info`. Cargo sets CARGO_FEATURE_<NAME> for each enabled
    // feature and reruns this script when the set changes.
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| {
            k.strip_prefix("CARGO_FEATURE_")
                .map(|f| f.to_ascii_lowercase().replace('_', "-"))
        })
        .filter(|f| f != "default")
        .collect();
    features.sort();
    let features = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    };
    println!("cargo:rustc-env=MANTA_FEATURES={features}");
}
