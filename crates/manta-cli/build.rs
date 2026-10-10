//! MAN-128: embeds the git commit into `manta_build_info`. Never fails the
//! build: a Docker context (`.dockerignore` excludes `.git`) or a source
//! tarball has no git metadata and reports "unknown" -- as does one unpacked
//! beneath an unrelated repository, whose HEAD is not manta's commit (MAN-83:
//! git's top level must be this workspace's root). `MANTA_GIT_SHA`
//! overrides (for release images that pass it as a build-arg).
//!
//! MAN-83: the same commit also feeds `manta --version` and the JSON
//! stream's `decoderVersion` (`manta-<version>+<sha>`), so the override must
//! be SemVer build metadata (dot-separated `[0-9A-Za-z-]` identifiers, at
//! most 64 characters); anything else is warned about and ignored. A full
//! git object name (40 or 64 hex digits, e.g. CI's `github.sha`) is cut to
//! 12 and lowercased, so it matches a native build of that commit; any
//! other override, such as a long numeric build ID, is kept as given.
//! This script also exports `MANTA_FEATURES`, the compiled-in Cargo feature
//! list, derived from Cargo's `CARGO_FEATURE_*` variables and never read
//! from the environment. Both names are still on `config.rs`'s
//! `ENV_IGNORED`: `cargo run` and `cargo test` export every `rustc-env`
//! value into the processes they start, and MAN-261 rejects unknown
//! `MANTA_*` names. A valid override also sets the `manta_git_sha_override`
//! cfg, which tests read to skip their check against the checkout's HEAD --
//! the runtime environment cannot tell them, for the same export reason.
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
/// empty, not UTF-8, or not build metadata -- the last two with a
/// `cargo:warning`, and the caller then falls through to `git` exactly as if
/// it were unset.
fn sha_override() -> Option<String> {
    let s = match std::env::var("MANTA_GIT_SHA") {
        Ok(s) if !s.is_empty() => s,
        Err(std::env::VarError::NotUnicode(raw)) => {
            println!("cargo:warning=MANTA_GIT_SHA={raw:?} is not UTF-8; ignoring it");
            return None;
        }
        _ => return None,
    };
    if !is_build_metadata(&s) {
        println!(
            "cargo:warning=MANTA_GIT_SHA={s:?} is not SemVer build metadata \
             (dot-separated [0-9A-Za-z-] identifiers, at most 64 characters); ignoring it"
        );
        return None;
    }
    if matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(s[..12].to_ascii_lowercase());
    }
    Some(s)
}

/// MAN-83: whether `git`, run in `crate_dir`, finds manta's own checkout. A
/// source tree with no `.git` of its own (an unpacked archive, a vendored
/// copy) built beneath an unrelated repository would otherwise report that
/// repository's HEAD as manta's commit, so the checkout's top level must be
/// the workspace root, two directories above this crate (`crates/manta-cli`).
/// `--show-cdup` is relative, so no absolute path (whose form differs
/// between Git for Windows, MSYS2 and Cygwin) is parsed. `pub` for
/// tests/build_identity.rs, which compiles this file as a module.
pub fn in_own_checkout(crate_dir: &std::path::Path) -> bool {
    crate_dir
        .to_str()
        .and_then(|dir| git(&["-C", dir, "rev-parse", "--show-cdup"]))
        .is_some_and(|up| up == "../../")
}

fn main() {
    println!("cargo:rerun-if-env-changed=MANTA_GIT_SHA");
    println!("cargo::rustc-check-cfg=cfg(manta_git_sha_override)");
    let overridden = sha_override();
    if overridden.is_some() {
        println!("cargo::rustc-cfg=manta_git_sha_override");
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let own_checkout = in_own_checkout(std::path::Path::new(&manifest_dir));
    let sha = overridden
        .or_else(|| {
            if own_checkout {
                git(&["rev-parse", "--short=12", "HEAD"])
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Rebuild when HEAD moves -- only for paths that exist (a missing
    // rerun-if-changed path makes Cargo rerun this script on every build),
    // and only in manta's own checkout. The paths `git rev-parse` returns
    // are relative to this script's cwd (the crate directory), so join
    // against CARGO_MANIFEST_DIR.
    if own_checkout {
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
    }
    println!("cargo:rustc-env=MANTA_GIT_SHA={sha}");

    // MAN-83: compiled-in optional features, for `--version` and
    // `manta_build_info`. Cargo sets CARGO_FEATURE_<NAME> for each enabled
    // feature and reruns this script when the set changes. `vars_os`, not
    // `vars`: the latter panics on any non-UTF-8 name or value the build
    // inherits, while the keys Cargo sets here are always UTF-8.
    let mut features: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| {
            k.to_str()?
                .strip_prefix("CARGO_FEATURE_")
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
