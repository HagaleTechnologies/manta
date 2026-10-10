//! MAN-83 acceptance: a support request is answerable from `--version`
//! alone, and every JSON spot names the build that produced it.
//!
//!   Scenario: --version identifies the exact build
//!   Scenario: Every emitted spot carries a real decoder version

use std::process::Command;

/// The child must not inherit `MANTA_*` from the test runner (MAN-261's
/// loader rejects unknown names), same as tests/cli.rs's `manta()`.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// What rustc compiled this test crate with, independent of build.rs's
/// CARGO_FEATURE_* scan. Extend when manta-cli gains a feature.
fn expected_features() -> String {
    let on: Vec<&str> = [
        ("hpsdr", cfg!(feature = "hpsdr")),
        ("soapy", cfg!(feature = "soapy")),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(f, _)| f)
    .collect();
    if on.is_empty() {
        "none".into()
    } else {
        on.join(",")
    }
}

/// `git rev-parse --short=12 HEAD` for this checkout, or None without git.
fn checkout_head() -> Option<String> {
    let out = Command::new("git")
        .args([
            "-C",
            env!("CARGO_MANIFEST_DIR"),
            "rev-parse",
            "--short=12",
            "HEAD",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

#[test]
fn version_flag_names_the_commit_and_features() {
    let want = format!(
        "manta {} (git {}; features: {})\n",
        env!("CARGO_PKG_VERSION"),
        env!("MANTA_GIT_SHA"),
        expected_features()
    );
    for flag in ["--version", "-V"] {
        let out = manta().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{flag}");
    }
}

/// The SHA is the checkout's real HEAD, not a constant, checked against git
/// rather than against build.rs. Skipped when the build was overridden
/// (MANTA_GIT_SHA set) or git is unavailable.
#[test]
fn version_flag_commit_is_the_checkouts_head() {
    if std::env::var_os("MANTA_GIT_SHA").is_some() {
        return;
    }
    let Some(head) = checkout_head() else { return };
    assert_eq!(env!("MANTA_GIT_SHA"), head);
    let out = manta().arg("--version").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("(git {head};")),
        "{stdout:?} does not name HEAD {head}"
    );
}
