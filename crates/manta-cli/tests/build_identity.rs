//! MAN-83 acceptance: a support request is answerable from `--version`
//! alone, and every JSON spot names the build that produced it.
//!
//!   Scenario: --version identifies the exact build
//!   Scenario: Every emitted spot carries a real decoder version

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
/// `CARGO_CFG_FEATURE` read. Extend when manta-cli gains a feature.
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

/// `git rev-parse --short=12 HEAD` for this checkout, or None without git
/// or when build.rs's own check finds the repository is not manta's (it then
/// embeds unknown). `build_script_ignores_an_enclosing_repository` covers
/// that check, so a check that always failed could not skip this one quietly.
/// Run through build.rs's `git_command`, which ignores an exported
/// `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` as build.rs did.
fn checkout_head() -> Option<String> {
    if !build_script::in_own_checkout(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))) {
        return None;
    }
    let out = build_script::git_command()
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
/// rather than against build.rs. Skipped when the build was overridden or
/// git is unavailable. The override is read from build.rs's cfg, not from
/// this process's `MANTA_GIT_SHA`: `cargo test` exports build.rs's
/// `rustc-env` values to every test, so that variable is always set here.
#[test]
fn version_flag_commit_is_the_checkouts_head() {
    if cfg!(manta_git_sha_override) {
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

/// build.rs itself, so its checkout check runs against real git layouts.
#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

/// A source tree with no `.git` of its own (an unpacked archive, a vendored
/// copy) built beneath an unrelated repository must not take that
/// repository's HEAD as manta's commit; a checkout rooted at the workspace
/// root still counts. Skipped when git is unavailable, or when this process
/// exports one of `build_script::REPO_ENV_VARS`, which the setup's own git
/// commands would then follow.
#[test]
fn build_script_ignores_an_enclosing_repository() {
    if build_script::REPO_ENV_VARS
        .iter()
        .any(|v| std::env::var_os(v).is_some())
    {
        return;
    }
    let outer = tempfile::tempdir().unwrap();
    let workspace = outer.path().join("manta-src");
    let crate_dir = workspace.join("crates").join("manta-cli");
    std::fs::create_dir_all(&crate_dir).unwrap();
    let git_init = |at: &std::path::Path| {
        Command::new("git")
            .args(["init", "-q"])
            .arg(at)
            .status()
            .is_ok_and(|s| s.success())
    };
    if !git_init(outer.path()) {
        return;
    }
    // Precondition: git's own discovery does walk up to the outer repo.
    let found = Command::new("git")
        .arg("-C")
        .arg(&crate_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .unwrap();
    assert!(found.status.success(), "{found:?}");
    assert!(
        !build_script::in_own_checkout(&crate_dir),
        "an enclosing repository was taken for manta's checkout"
    );
    assert!(git_init(&workspace));
    assert!(
        build_script::in_own_checkout(&crate_dir),
        "a checkout rooted at the workspace root was rejected"
    );
}

/// Runs the `#[ignore]`d test `name` in a child process of this test
/// binary with `envs` added, so a build.rs function sees an inherited
/// environment that other tests, running in parallel here, must not.
/// "1 passed" catches a misspelt `name`, which would run nothing.
fn run_child_test(name: &str, envs: &[(&str, &std::ffi::OsStr)]) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored"])
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{name}: {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A build that inherits `GIT_DIR` and `GIT_WORK_TREE` for another
/// repository whose work tree is manta's workspace must not take that
/// repository's HEAD, with or without a `.git` of manta's own. Skipped
/// when git is unavailable, or when this process already exports the
/// variables, which the setup's own git commands would then follow.
#[test]
fn build_script_ignores_exported_repository_variables() {
    if build_script::REPO_ENV_VARS
        .iter()
        .any(|v| std::env::var_os(v).is_some())
    {
        return;
    }
    let git_ok = |at: &std::path::Path, args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .is_ok_and(|s| s.success())
    };
    let other = tempfile::tempdir().unwrap();
    if !git_ok(other.path(), &["init", "-q"])
        || !git_ok(
            other.path(),
            &["commit", "-q", "--allow-empty", "-m", "other"],
        )
    {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let workspace = src.path().join("manta-src");
    let crate_dir = workspace.join("crates").join("manta-cli");
    std::fs::create_dir_all(&crate_dir).unwrap();
    let other_git_dir = other.path().join(".git");
    // Precondition: plain git, given these variables, takes the other
    // repository for one rooted at the workspace root.
    let cdup = Command::new("git")
        .arg("-C")
        .arg(&crate_dir)
        .args(["rev-parse", "--show-cdup"])
        .env("GIT_DIR", &other_git_dir)
        .env("GIT_WORK_TREE", &workspace)
        .output()
        .unwrap();
    assert!(cdup.status.success(), "{cdup:?}");
    assert_eq!(String::from_utf8_lossy(&cdup.stdout).trim(), "../../");
    let envs = [
        ("GIT_DIR", other_git_dir.as_os_str()),
        ("GIT_WORK_TREE", workspace.as_os_str()),
        ("BUILD_IDENTITY_CRATE_DIR", crate_dir.as_os_str()),
    ];
    // An unpacked archive: no `.git` of its own, so no commit to report.
    run_child_test("exported_repository_variables_child", &envs);
    // A checkout of its own: its HEAD, not the other repository's.
    assert!(git_ok(&workspace, &["init", "-q"]));
    assert!(git_ok(
        &workspace,
        &["commit", "-q", "--allow-empty", "-m", "manta"]
    ));
    let head = Command::new("git")
        .arg("-C")
        .arg(&workspace)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success(), "{head:?}");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let mut envs = envs.to_vec();
    envs.push(("BUILD_IDENTITY_HEAD", std::ffi::OsStr::new(&head)));
    run_child_test("exported_repository_variables_child", &envs);
}

/// Child half of `build_script_ignores_exported_repository_variables`,
/// which supplies the environment; a no-op without it.
#[test]
#[ignore = "run by build_script_ignores_exported_repository_variables"]
fn exported_repository_variables_child() {
    let Some(crate_dir) = std::env::var_os("BUILD_IDENTITY_CRATE_DIR") else {
        return;
    };
    let crate_dir = std::path::Path::new(&crate_dir);
    let Ok(want) = std::env::var("BUILD_IDENTITY_HEAD") else {
        assert!(
            !build_script::in_own_checkout(crate_dir),
            "an exported GIT_DIR/GIT_WORK_TREE was taken for manta's checkout"
        );
        return;
    };
    assert!(build_script::in_own_checkout(crate_dir));
    let out = build_script::git_command()
        .arg("-C")
        .arg(crate_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), want);
}

/// Cargo passes an inherited `CARGO_FEATURE_*` through to build scripts, so
/// `CARGO_FEATURE_SOAPY=1 cargo build` must not list soapy uncompiled. The
/// child sees what Cargo sets for an hpsdr-only build plus a leaked soapy.
#[test]
fn build_script_ignores_inherited_feature_variables() {
    let one = std::ffi::OsStr::new("1");
    run_child_test(
        "inherited_feature_variables_child",
        &[
            ("CARGO_CFG_FEATURE", std::ffi::OsStr::new("hpsdr")),
            ("CARGO_FEATURE_HPSDR", one),
            ("CARGO_FEATURE_SOAPY", one),
            ("BUILD_IDENTITY_FEATURES", std::ffi::OsStr::new("hpsdr")),
        ],
    );
}

/// Child half of `build_script_ignores_inherited_feature_variables`; a
/// no-op without its environment.
#[test]
#[ignore = "run by build_script_ignores_inherited_feature_variables"]
fn inherited_feature_variables_child() {
    let Ok(want) = std::env::var("BUILD_IDENTITY_FEATURES") else {
        return;
    };
    assert_eq!(build_script::compiled_features(), want);
}

/// Kills and reaps the daemon on drop, including on a failing assertion, so
/// a failed test never leaks a `manta` process (node_health_acceptance.rs's
/// `ChildGuard`).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A minimal valid `[server]` table -- loopback only, every port 0
/// (multi-agent hygiene: never bind a fixed port), as in tests/cli.rs.
const SERVER_TOML: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

/// Scenario 2, end to end: a real `manta run` over a synthetic recording,
/// read off the JSON stream's wire. The stream keeps no history for late
/// subscribers and dedupe suppresses re-spots for 600 s of sample time, so
/// the client connects as soon as the `listening:` banner names the bound
/// port -- about 2 ms after it, against ~0.9 s to the first spot.
#[test]
fn json_spots_carry_the_builds_decoder_version() {
    let dir = tempfile::tempdir().unwrap();
    // Full-length V1 (120 s), not tests/cli.rs's 30 s `short_v1()`: its
    // first W1AW CQ spot still lands ~21 s of sample time in, but the
    // replay then runs for ~100 s more. At EOF the daemon signals shutdown,
    // and json_stream drops a client still inside its 500 ms wall-clock
    // WebSocket-detection peek; a 30 s replay ends under a second after the
    // banner here, close enough to that peek for a faster runner to lose
    // the spot.
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));
    let cfg = dir.path().join("server.toml");
    std::fs::write(&cfg, SERVER_TOML).unwrap();

    let child = manta()
        .arg("run")
        .arg("--source")
        .arg(&wav)
        .args(["--source-iq", "--dial-freq-hz", "14000000", "--config"])
        .arg(&cfg)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn manta run");
    let mut guard = ChildGuard(child);

    // The banner names the bound port; ANSI colour codes sit outside the
    // `json=<addr>` token.
    let port_re = regex::Regex::new(r"json=127\.0\.0\.1:(\d+)").unwrap();
    let mut lines = BufReader::new(guard.0.stderr.take().unwrap()).lines();
    let mut before = Vec::new();
    let port: u16 = loop {
        let Some(line) = lines.next() else {
            panic!(
                "manta run exited before its listening: banner; stderr:\n{}",
                before.join("\n")
            );
        };
        let line = line.unwrap();
        if line.contains("listening:") {
            if let Some(c) = port_re.captures(&line) {
                break c[1].parse().unwrap();
            }
        }
        before.push(line);
    };
    // Keep draining stderr so a full pipe can never block the daemon.
    std::thread::spawn(move || lines.map_while(Result::ok).for_each(drop));

    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut first = String::new();
    let n = BufReader::new(stream)
        .read_line(&mut first)
        .expect("no JSON spot within 60 s");
    assert!(
        n > 0,
        "the JSON stream closed before any spot: the client connected too late \
         (the stream keeps no history), or the replay produced no spot"
    );
    let spot: serde_json::Value = serde_json::from_str(&first).unwrap();
    let want = format!(
        "manta-{}+{}",
        env!("CARGO_PKG_VERSION"),
        env!("MANTA_GIT_SHA")
    );
    assert_eq!(spot["decoderVersion"], want.as_str(), "spot: {first}");
    if !cfg!(manta_git_sha_override) {
        if let Some(head) = checkout_head() {
            assert!(want.ends_with(&format!("+{head}")), "{want} vs HEAD {head}");
        }
    }
}
