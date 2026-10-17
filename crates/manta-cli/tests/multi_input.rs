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
    // MAN-13: the banner names the inputs, not one source.
    assert!(stderr.contains("listening: inputs=a,b"), "{stderr}");
    assert!(stderr.contains("station=K5TR"), "{stderr}");
    let banner = stderr
        .lines()
        .find(|l| l.contains("listening: inputs=a,b"))
        .unwrap();
    assert!(
        !banner.contains("source=") && !banner.contains("sample_rate_hz="),
        "the multi-input banner leaves the per-source fields to each input's \
         own line: {banner}"
    );
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

/// MAN-13 Phase 5: with two or more inputs the banner lists them and each
/// input prints its own line and its own ready line, naming its own rate.
#[test]
fn banner_lists_every_input_and_each_input_gets_a_ready_line() {
    let out = run_config(&overlap_config());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    for (name, rate) in [("a", "96000"), ("b", "192000")] {
        assert!(
            stderr.contains(&format!(
                "ready: decoding source={name} sample_rate_hz={rate}"
            )),
            "no ready line with its own rate for input {name}: {stderr}"
        );
    }
}

/// MAN-13: one input keeps today's banner, source name and metrics label.
#[test]
fn single_input_banner_and_metrics_labels_are_unchanged() {
    let (dir, a, _) = fixtures();
    let config = write_config(
        dir.path(),
        "single.toml",
        &format!(
            "[server]\nstation_callsign = \"K5TR\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\n\
             metrics_port = 0\n\n[input]\ntype = \"file\"\npath = \"{}\"\niq = true\n",
            toml_path(a)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("listening: source=file sample_rate_hz=96000 dial_frequency=14000.0 kHz"),
        "{stderr}"
    );
    assert!(
        stderr.contains("ready: decoding source=file sample_rate_hz=96000"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("inputs="),
        "a one-input run must not grow the multi-input fields: {stderr}"
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

/// Codex on PR 313: a source flag names a standalone receiver and ignores
/// the configured entries, so a one-receiver command given one needs no
/// `--input` with several entries.
#[test]
fn a_source_flag_needs_no_input_with_several_entries() {
    let (_, _, b) = fixtures();
    let out = manta()
        .args(["check", "--duration", "1", "--source-iq", "--source"])
        .arg(b)
        .arg("--config")
        .arg(overlap_config())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("192000"), "{stdout}");
}

/// Codex on PR 313: a recording came from one receiver, and `decode`
/// cannot tell which of several `[[input]]` entries it was, so entries that
/// disagree on the correction are refused rather than the first one's
/// applied; `--freq-correction-ppm` gives the right one.
#[test]
fn decode_refuses_several_inputs_without_a_correction() {
    let (dir, a, b) = fixtures();
    let two_ppm = |name: &str, ppm_a: f64, ppm_b: f64| {
        write_config(
            dir.path(),
            name,
            &format!(
                "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
                 freq_correction_ppm = {ppm_a:?}\n\n\
                 [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
                 freq_correction_ppm = {ppm_b:?}\n",
                toml_path(a),
                toml_path(b)
            ),
        )
    };
    let config = two_ppm("decode-two-ppm.toml", 2.0, -1.5);
    let missing = dir.path().join("no-such-recording.wav");
    let out = manta()
        .args(["decode", "--config"])
        .arg(&config)
        .arg(&missing)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "manta decode cannot tell which of {}'s 2 inputs (a, b) recorded {}: pass \
             --freq-correction-ppm with that receiver's correction (0 for none)",
            config.display(),
            missing.display()
        )),
        "{stderr}"
    );
    let named = manta()
        .args(["decode", "--freq-correction-ppm", "-1.5", "--config"])
        .arg(&config)
        .arg(&missing)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&named.stderr);
    assert!(!stderr.contains("cannot tell which"), "{stderr}");
    assert!(stderr.contains("no-such-recording.wav"), "{stderr}");
    // Entries that agree leave nothing to choose.
    let agreed = manta()
        .args(["decode", "--config"])
        .arg(two_ppm("decode-same-ppm.toml", 1.0, 1.0))
        .arg(&missing)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&agreed.stderr);
    assert!(!stderr.contains("cannot tell which"), "{stderr}");
    assert!(
        stderr.contains("note: decode applies only freq_correction_ppm from the 2 [[input]]"),
        "{stderr}"
    );
}

/// MAN-124 (Codex, PR 313): with several inputs `--log-format json` keeps
/// every stderr line a JSON record — the grouped decoded text rides a
/// `manta::text` event rather than a bare stderr line a collector would
/// reject.
#[test]
fn json_log_format_keeps_decoded_text_a_json_record() {
    let out = manta()
        .args(["run", "--config"])
        .arg(overlap_config())
        .args(["--log-format", "json"])
        .env("RUST_LOG", "info")
        .env_remove("NO_COLOR")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let mut saw_text = false;
    for line in stderr.lines() {
        let record: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stderr line is not one JSON object ({e}): {line:?}"));
        saw_text |= record["message"]
            .as_str()
            .is_some_and(|m| m.contains("CQ CQ DE W1AW"));
    }
    assert!(
        saw_text,
        "no decoded-text record on the manta::text target: {stderr}"
    );
}

/// MAN-196 (Codex, PR 313): a decimated `[[input]]` states its usable
/// passband once at startup, as the one-input path does, while the
/// undecimated input beside it says nothing.
#[test]
fn a_decimated_input_states_its_usable_passband() {
    let (dir, a, b) = fixtures();
    let config = write_config(
        dir.path(),
        "decimated.toml",
        &format!(
            "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
             capture_rate_hz = 48000.0\n",
            toml_path(a),
            toml_path(b)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.contains("decimated capture 48000 Hz: usable passband"))
            .count(),
        1,
        "exactly one usable-passband line, for input b only: {stderr}"
    );
}

/// MAN-234 (Codex, PR 313): one poisoned `[[input]]` warns naming it and
/// keeps decoding — its samples are replaced with silence — while the
/// clean input beside it warns nothing.
#[test]
fn bad_samples_in_one_input_warn_naming_it() {
    let (dir, a, _) = fixtures();
    let (samples, _) = render_scene(
        &[station("CQ CQ DE ZL2XYZ ZL2XYZ K", 25_000.0)],
        192_000.0,
        30.0,
        Some(22),
    )
    .unwrap();
    let mut poisoned = samples;
    poisoned[1_000_000].re = f32::NAN;
    poisoned[3_000_000].im = f32::INFINITY;
    let b = manta_testkit::wav::write_fixture(
        dir.path(),
        "poison_b",
        &poisoned,
        192_000.0,
        14_060_000.0,
    )
    .unwrap();
    let config = write_config(
        dir.path(),
        "poisoned.toml",
        &format!(
            "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"{}\"\niq = true\n",
            toml_path(a),
            toml_path(&b)
        ),
    );
    let out = run_config(&config);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: b delivered ")
            && stderr.contains("replaced with silence, decoding continues"),
        "no first-sample warning naming input b: {stderr}"
    );
    assert!(
        !stderr.contains("warning: a delivered "),
        "the clean input warned: {stderr}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(spot_calls(&stdout).get("ZL2XYZ"), Some(&1), "{stdout}");
}
