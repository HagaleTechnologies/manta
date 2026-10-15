//! MAN-13 scenario 1 end to end: one `manta run` over two file inputs whose
//! segments overlap prints each station once on stdout -- the station both
//! receivers hear included -- plus the config refusals that name an input.
//! File inputs only, so it runs on every platform.

use manta_testkit::scene::{render_scene, SignalSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// `manta` with no `MANTA_*` variable from the test runner's environment.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn station(text: &str, offset_hz: f64) -> SignalSpec {
    SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: text.into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    }
}

const W1AW_HZ: f64 = 14_012_340.0;

/// The overlap fixture of the MAN-13 plan: `seg_a.wav` 96 kS/s at
/// 14.000 MHz (W1AW, JA1ABC), `seg_b.wav` 192 kS/s at 14.060 MHz (W1AW,
/// ZL2XYZ), 120 s, 20 dB, 20 WPM, each with its frequency sidecar.
fn fixtures() -> &'static (tempfile::TempDir, PathBuf, PathBuf) {
    static F: OnceLock<(tempfile::TempDir, PathBuf, PathBuf)> = OnceLock::new();
    F.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = render_scene(
            &[
                station("CQ CQ DE W1AW W1AW K", W1AW_HZ - 14_000_000.0),
                station("CQ CQ DE JA1ABC JA1ABC K", -20_000.0),
            ],
            96_000.0,
            120.0,
            Some(11),
        )
        .unwrap();
        let a = manta_testkit::wav::write_fixture(dir.path(), "seg_a", &a, 96_000.0, 14_000_000.0)
            .unwrap();
        let (b, _) = render_scene(
            &[
                station("CQ CQ DE W1AW W1AW K", W1AW_HZ - 14_060_000.0),
                station("CQ CQ DE ZL2XYZ ZL2XYZ K", 25_000.0),
            ],
            192_000.0,
            120.0,
            Some(22),
        )
        .unwrap();
        let b = manta_testkit::wav::write_fixture(dir.path(), "seg_b", &b, 192_000.0, 14_060_000.0)
            .unwrap();
        (dir, a, b)
    })
}

fn toml_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn write_config(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// Written once: tests run in parallel, and rewriting the file while
/// another test's `manta` reads it would hand that run a truncated config.
fn overlap_config() -> PathBuf {
    static CONFIG: OnceLock<PathBuf> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let (dir, a, b) = fixtures();
            write_config(
                dir.path(),
                "overlap.toml",
                &format!(
                    "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
                     [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n",
                    toml_path(a),
                    toml_path(b)
                ),
            )
        })
        .clone()
}

fn run_config(config: &Path) -> Output {
    manta()
        .args(["run", "--config"])
        .arg(config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap()
}

fn spot_calls(stdout: &str) -> BTreeMap<String, usize> {
    let mut calls = BTreeMap::new();
    for line in stdout.lines() {
        let call = line
            .strip_prefix("SPOT: ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("stdout line is not a SPOT line: {line:?}"));
        *calls.entry(call.to_string()).or_insert(0) += 1;
    }
    calls
}

#[test]
fn two_overlapping_file_inputs_spot_the_shared_station_once() {
    let out = run_config(&overlap_config());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let want: BTreeMap<String, usize> = [("JA1ABC", 1), ("W1AW", 1), ("ZL2XYZ", 1)]
        .iter()
        .map(|(c, n)| (c.to_string(), *n))
        .collect();
    assert_eq!(spot_calls(&stdout), want, "{stdout}\n{stderr}");
    for name in ["a", "b"] {
        assert!(
            stderr.contains(&format!("ready: decoding source={name}")),
            "no ready line for input {name}: {stderr}"
        );
    }
}

#[test]
fn multi_input_daemon_serves_one_stream() {
    let (dir, a, b) = fixtures();
    let config = write_config(
        dir.path(),
        "daemon.toml",
        &format!(
            "[server]\nstation_callsign = \"K5TR\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\n\
             metrics_port = 0\n\n\
             [[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n",
            toml_path(a),
            toml_path(b)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let want: BTreeMap<String, usize> = [("JA1ABC", 1), ("W1AW", 1), ("ZL2XYZ", 1)]
        .iter()
        .map(|(c, n)| (c.to_string(), *n))
        .collect();
    assert_eq!(spot_calls(&stdout), want, "{stdout}\n{stderr}");
    assert!(stderr.contains("listening: source=a,b"), "{stderr}");
    assert!(stderr.contains("station=K5TR"), "{stderr}");
    // One line per input after it opens, naming its type, rate and dial.
    assert!(
        stderr.contains("input a: type=file sample_rate_hz=96000 dial_frequency=14000.0 kHz"),
        "{stderr}"
    );
    assert!(
        stderr.contains("input b: type=file sample_rate_hz=192000 dial_frequency=14060.0 kHz"),
        "{stderr}"
    );
}

#[test]
fn multi_input_replay_stdout_is_byte_identical_across_three_runs() {
    let config = overlap_config();
    let first = run_config(&config);
    assert!(first.status.success());
    for _ in 0..2 {
        assert_eq!(run_config(&config).stdout, first.stdout);
    }
}

#[test]
fn file_input_without_a_frequency_is_refused_naming_it() {
    let (dir, a, _) = fixtures();
    // A copy of seg_a without its sidecar.
    let bare = dir.path().join("bare.wav");
    std::fs::copy(a, &bare).unwrap();
    let config = write_config(
        dir.path(),
        "bare.toml",
        &format!(
            "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\n",
            toml_path(a),
            toml_path(&bare)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "input \"b\" ({}) carries no frequency of its own: set center_freq_hz in its \
             [[input]] entry",
            bare.display()
        )),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn an_input_that_fails_to_open_stops_startup_naming_it() {
    let (dir, a, _) = fixtures();
    let missing = dir.path().join("missing.wav");
    let config = write_config(
        dir.path(),
        "missing.toml",
        &format!(
            "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
             center_freq_hz = 14060000.0\n",
            toml_path(a),
            toml_path(&missing)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("error: input \"b\" failed to open: "),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn config_check_accepts_two_named_inputs() {
    let out = manta()
        .args(["config", "check", "--config"])
        .arg(overlap_config())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("input a: type=file"), "{stdout}");
    assert!(stdout.contains("input b: type=file"), "{stdout}");
}

#[test]
fn doctor_refuses_several_inputs_without_input() {
    let config = overlap_config();
    let out = manta()
        .args(["doctor", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "manta doctor checks one receiver at a time and {} has 2 inputs: pass --input a or \
             --input b",
            config.display()
        )),
        "{stderr}"
    );
}

#[test]
fn check_with_input_reports_that_inputs_rate() {
    let config = overlap_config();
    let out = manta()
        .args(["check", "--duration", "1", "--input", "b", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("192000"), "{stdout}");
    let unknown = manta()
        .args(["check", "--input", "nope", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains(&format!(
            "--input \"nope\" is not one of {}'s inputs (a, b)",
            config.display()
        )),
        "{stderr}"
    );
}
