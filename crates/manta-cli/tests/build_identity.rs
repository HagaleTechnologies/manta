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
    if std::env::var_os("MANTA_GIT_SHA").is_none() {
        if let Some(head) = checkout_head() {
            assert!(want.ends_with(&format!("+{head}")), "{want} vs HEAD {head}");
        }
    }
}
