//! MAN-128: embeds the git commit into `manta_build_info`. Never fails the
//! build: a Docker context (`.dockerignore` excludes `.git`) or a source
//! tarball has no git metadata and reports "unknown". `MANTA_GIT_SHA`
//! overrides (for release images that pass it as a build-arg).
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=MANTA_GIT_SHA");
    let sha = std::env::var("MANTA_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
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
}
