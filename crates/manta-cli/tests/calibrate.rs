//! MAN-127: `manta calibrate` end to end, on hermetic IQ WAV fixtures whose
//! carrier frequency is known exactly.

mod common;

use num_complex::Complex32;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const FS: f64 = 48_000.0;
const CENTRE: f64 = 9_998_500.0;
/// A 10 MHz carrier as a receiver 2.5 ppm fast would see it.
const CARRIER_2_5_PPM: f64 = 10e6 / (1.0 + 2.5e-6);
const SECS: f64 = 12.0;

fn manta() -> Command {
    let mut cmd = common::manta();
    cmd.stdin(Stdio::null());
    cmd
}

/// Seeded xorshift64 + Box-Muller complex Gaussian noise, sigma per axis.
fn noise(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
    let mut s = seed.max(1);
    let mut uniform = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    };
    (0..n)
        .map(|_| {
            let r = (-2.0 * uniform().ln()).sqrt() * sigma;
            let t = 2.0 * std::f64::consts::PI * uniform();
            Complex32::new((r * t.cos()) as f32, (r * t.sin()) as f32)
        })
        .collect()
}

/// `SECS` of a unit carrier at absolute `carrier_hz` (none when `None`)
/// plus noise at SNR +10 dB in 2500 Hz, as seen by a receiver at `centre`.
fn samples(carrier_hz: Option<f64>, centre: f64) -> Vec<Complex32> {
    let n = (SECS * FS) as usize;
    let sigma = (0.1 * FS / 2500.0 / 2.0f64).sqrt();
    let mut s = noise(n, sigma, 42);
    if let Some(hz) = carrier_hz {
        let f = hz - centre;
        for (i, x) in s.iter_mut().enumerate() {
            let phi = 2.0 * std::f64::consts::PI * f * i as f64 / FS;
            *x += Complex32::new(phi.cos() as f32, phi.sin() as f32);
        }
    }
    s
}

fn fixture(dir: &Path, carrier_hz: Option<f64>, centre: f64) -> PathBuf {
    manta_testkit::wav::write_fixture(dir, "ref", &samples(carrier_hz, centre), FS, centre).unwrap()
}

const FILE_INPUT: &str = "[input]\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n";

fn write_cfg(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("manta.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn calibrate(cfg: &Path, extra: &[&str]) -> Output {
    manta()
        .args(["calibrate", "--duration", "12", "--config"])
        .arg(cfg)
        .args(extra)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn calibrate_reports_the_ppm_of_the_configured_receiver() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    for needle in [
        "reference: 10000000.0 Hz, time standard (WWV, WWVH, BPM)",
        "the receiver reads 25.0 Hz low",
        "correction: freq_correction_ppm = +2.50",
        "not saved: no terminal to confirm on",
    ] {
        assert!(stdout.contains(needle), "no {needle:?} in {stdout}");
    }
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}

#[test]
fn calibrate_json_reports_the_measurement() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ppm = v["result"]["freq_correction_ppm"].as_f64().unwrap();
    assert!((ppm - 2.5).abs() < 0.02, "{ppm}");
    assert_eq!(v["references"][0]["status"], "measured");
    assert_eq!(v["written"], false);
    assert_eq!(v["config_path"], cfg.display().to_string());
}

#[test]
fn calibrate_write_saves_only_that_key() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let original = "# The node's receiver.\n[input]\ntype = \"file\"   # a recording\n\
                    path = \"ref.wav\"\niq = true\n\n# end\n";
    let cfg = write_cfg(dir.path(), original);
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.contains("saved: freq_correction_ppm = 2.5 in [input] of"),
        "{stdout}"
    );
    let edited = std::fs::read_to_string(&cfg).unwrap();
    let added: Vec<&str> = edited
        .lines()
        .filter(|l| !original.lines().any(|o| o == *l))
        .collect();
    assert_eq!(added, vec!["freq_correction_ppm = 2.5"], "{edited}");
    let kept: Vec<&str> = edited
        .lines()
        .filter(|l| *l != "freq_correction_ppm = 2.5")
        .collect();
    assert_eq!(kept, original.lines().collect::<Vec<_>>(), "{edited}");

    let check = manta()
        .args(["config", "check", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", text(&check.stderr));
    assert!(
        text(&check.stdout).contains("freq_correction_ppm=2.5"),
        "{}",
        text(&check.stdout)
    );
}

#[test]
fn calibrate_write_replaces_rather_than_adds() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(
        dir.path(),
        &format!("{FILE_INPUT}freq_correction_ppm = 5.0\n"),
    );
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("(was 5.0)"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        format!("{FILE_INPUT}freq_correction_ppm = 2.5\n")
    );
}

#[test]
fn calibrate_reports_already_set() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = format!("{FILE_INPUT}freq_correction_ppm = 2.5\n");
    let cfg = write_cfg(dir.path(), &body);
    let out = calibrate(&cfg, &["--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.contains("not saved: ") && stdout.contains("already has freq_correction_ppm = 2.5"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

#[test]
fn calibrate_write_without_config_fails_before_measuring() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let out = manta()
        .args([
            "calibrate",
            "--duration",
            "12",
            "--source-iq",
            "--write",
            "--source",
        ])
        .arg(&wav)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--write needs a config file"),
        "{}",
        text(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
}

#[test]
fn calibrate_write_refuses_a_replaced_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = 1\nfreq_hz = 7030000.0\n";
    let cfg = write_cfg(dir.path(), body);
    let out = calibrate(
        &cfg,
        &["--source-iq", "--write", "--source", wav.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("--write refuses to save a measurement taken with input overrides: --source, --source-iq."),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

#[test]
fn calibrate_write_refuses_a_receiver_the_environment_selects() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = 1\nfreq_hz = 7030000.0\n";
    let cfg = write_cfg(dir.path(), body);
    let out = manta()
        .env("MANTA_INPUT_TYPE", "file")
        .env("MANTA_INPUT_PATH", &wav)
        .env("MANTA_INPUT_IQ", "true")
        .args(["calibrate", "--duration", "12", "--write", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("--write refuses to save a measurement taken with input overrides: MANTA_INPUT_IQ, MANTA_INPUT_PATH, MANTA_INPUT_TYPE."),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

/// A typed `[input]` reading the IQ fixture as audio (`iq` unset): an IQ
/// override measures something that config never opens.
#[test]
fn calibrate_write_refuses_an_iq_override_of_an_audio_file_input() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = format!(
        "[input]\ntype = \"file\"\npath = {:?}\n",
        wav.to_str().unwrap()
    );
    let cfg = write_cfg(dir.path(), &body);
    let out = calibrate(&cfg, &["--source-iq", "--write"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "--write refuses to save a measurement taken with input overrides: --source-iq."
        ),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);

    let out = manta()
        .env("MANTA_INPUT_IQ", "true")
        .args(["calibrate", "--duration", "12", "--write", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(
            "--write refuses to save a measurement taken with input overrides: MANTA_INPUT_IQ."
        ),
        "{stderr}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body);
}

/// Deny-by-default (MAN-127): any input override refuses `--write`,
/// including a centre-frequency relabel and a `MANTA_INPUT_*` name manta
/// does not know (the prefix rule, checked before the config loads), and
/// the refusal says to persist them in the config first.
#[test]
fn calibrate_write_refuses_any_input_override() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let body = format!(
        "[input]\ntype = \"file\"\npath = {:?}\niq = true\n",
        wav.to_str().unwrap()
    );
    let cfg = write_cfg(dir.path(), &body);
    type Case<'a> = (&'a [(&'a str, &'a str)], &'a [&'a str], &'a str);
    let cases: [Case; 3] = [
        (&[], &["--dial-freq-hz", "9998510"], "--dial-freq-hz"),
        (
            &[("MANTA_INPUT_CENTER_FREQ_HZ", "9998510")],
            &[],
            "MANTA_INPUT_CENTER_FREQ_HZ",
        ),
        (&[("MANTA_INPUT_FOO", "1")], &[], "MANTA_INPUT_FOO"),
    ];
    for (env, flags, named) in cases {
        let mut cmd = manta();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd
            .args(["calibrate", "--duration", "12", "--write", "--config"])
            .arg(&cfg)
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{named}: {}", text(&out.stderr));
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains(&format!(
                "--write refuses to save a measurement taken with input overrides: {named}."
            )),
            "{stderr}"
        );
        assert!(
            stderr.contains("persist these settings in [input] of the config file"),
            "{stderr}"
        );
        assert!(out.stdout.is_empty(), "{named}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), body, "{named}");
    }

    // No overrides: the same config is measured and saved.
    let out = calibrate(&cfg, &["--write"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let saved = std::fs::read_to_string(&cfg).unwrap();
    assert!(saved.starts_with(&body), "{saved}");
    assert!(saved.contains("freq_correction_ppm = "), "{saved}");
}

#[test]
fn calibrate_explains_an_empty_passband() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), None, 7_030_000.0);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("no known reference frequency is usable") && stderr.contains("--tune-hz"),
        "{stderr}"
    );
}

#[test]
fn calibrate_failed_measurement_prints_the_report_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), None, CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("reference: none measured"), "{stdout}");
    assert!(stdout.contains("tried: 10000000.0 Hz"), "{stdout}");
    assert!(
        text(&out.stderr).contains("no reference carrier was measured"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_refuses_tune_hz_on_a_recording() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--tune-hz", "9998500"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--tune-hz cannot retune a recording"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_needs_an_absolute_frequency() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    std::fs::remove_file(dir.path().join("ref.json")).unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("absolute frequency"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_rejects_duration_below_10() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = manta()
        .args(["calibrate", "--duration", "5", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--duration must be between 10 and 3600 seconds, got 5"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_reference_hz_measures_an_operator_carrier() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(10_001_000.0), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = calibrate(&cfg, &["--json", "--reference-hz", "10001000"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["references"].as_array().unwrap().len(), 1);
    assert_eq!(v["references"][0]["kind"], "operator");
    assert_eq!(v["references"][0]["status"], "measured");
    let ppm = v["result"]["freq_correction_ppm"].as_f64().unwrap();
    assert!(ppm.abs() < 0.02, "{ppm}");
}

#[test]
fn calibrate_is_deterministic_on_a_file() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let a = calibrate(&cfg, &["--json"]);
    let b = calibrate(&cfg, &["--json"]);
    assert!(a.status.success());
    assert!(!a.stdout.is_empty());
    assert_eq!(a.stdout, b.stdout);
}

#[test]
fn calibrate_warns_when_the_environment_overrides_the_file() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let out = manta()
        .env("MANTA_INPUT_FREQ_CORRECTION_PPM", "1")
        .args(["calibrate", "--duration", "12", "--write", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("MANTA_INPUT_FREQ_CORRECTION_PPM is set and overrides"),
        "{stderr}"
    );
    assert!(
        text(&out.stdout).contains("configured: 1.00 ppm, from MANTA_INPUT_FREQ_CORRECTION_PPM"),
        "{}",
        text(&out.stdout)
    );
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = 2.5"));
}

#[cfg(unix)]
#[test]
fn calibrate_write_keeps_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = calibrate(&cfg, &["--write"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = 2.5"));
}

#[test]
fn top_level_help_lists_calibrate() {
    let out = manta().arg("--help").output().unwrap();
    assert!(out.status.success());
    // MAN-133's `completions` (a longer name than `calibrate`) widened
    // clap's Commands alignment column, so the row is matched with the
    // whitespace collapsed rather than pinning the gap width.
    let help = text(&out.stdout);
    let line = help
        .lines()
        .find(|l| l.trim_start().starts_with("calibrate"))
        .unwrap_or_else(|| panic!("no calibrate row in {help}"));
    assert_eq!(
        line.split_whitespace().collect::<Vec<_>>().join(" "),
        "calibrate Measure the receiver's frequency error against a known signal",
        "{help}"
    );
}

#[test]
fn calibrate_help_names_every_flag() {
    let out = manta().args(["calibrate", "--help"]).output().unwrap();
    assert!(out.status.success());
    let help = text(&out.stdout);
    for flag in [
        "--duration",
        "--reference-hz",
        "--search-ppm",
        "--tune-hz",
        "--write",
        "--json",
        "--config",
        "--source",
        "--kiwi-host",
        "--dial-freq-hz",
        "--from-rbn",
        "--addr",
    ] {
        assert!(help.contains(flag), "no {flag} in {help}");
    }
}

/// MAN-13: with several `[[input]]` entries, `--input NAME --write` saves
/// into that entry, and the save's reload check reads that entry too.
#[test]
fn calibrate_write_saves_into_the_named_input_entry() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let original = "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n\
                    freq_correction_ppm = 0.0\n\n[[input]]\nname = \"b\"\ntype = \"file\"\n\
                    path = \"ref.wav\"\niq = true\nfreq_correction_ppm = 0.5 # old\n";
    let cfg = write_cfg(dir.path(), original);
    let out = calibrate(&cfg, &["--input", "b", "--write"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        original.replace(
            "freq_correction_ppm = 0.5 # old",
            "freq_correction_ppm = 2.5 # old"
        )
    );
}

/// MAN-392: a source flag beside several `[[input]]` entries measures a
/// standalone receiver; calibrate reports the measurement in both output
/// forms, says nothing is configured for it, and leaves the file as it is.
#[test]
fn calibrate_a_source_flag_beside_several_inputs_reports_the_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path(), Some(CARRIER_2_5_PPM), CENTRE);
    let original = "[[input]]\nname = \"20m\"\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n\n\
                    [[input]]\nname = \"40m\"\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n";
    let cfg = write_cfg(dir.path(), original);
    let source = ["--source-iq", "--source", wav.to_str().unwrap()];

    let out = calibrate(&cfg, &source);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    for needle in [
        "correction: freq_correction_ppm = +2.50",
        "configured: 0.00 ppm, nothing set",
        "not saved: input overrides were in effect (--source, --source-iq)",
    ] {
        assert!(stdout.contains(needle), "no {needle:?} in {stdout}");
    }
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);

    let out = calibrate(&cfg, &[&source[..], &["--json"]].concat());
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ppm = v["result"]["freq_correction_ppm"].as_f64().unwrap();
    assert!((ppm - 2.5).abs() < 0.02, "{ppm}");
    assert_eq!(v["written"], false);
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

/// MAN-334: the retune hint for a KiwiSDR window centred at 7.030 MHz
/// (+/-5 kHz).
const KIWI_7030_HINT: &str = "no known reference frequency is usable in this receiver's \
    passband (7025000.0 to 7035000.0 Hz); retune it for the measurement, e.g. --tune-hz \
    4998500 for the 5000000.0 Hz time standard (WWV, WWVH, BPM) or --tune-hz 9998500 for the \
    10000000.0 Hz time standard (WWV, WWVH, BPM), or name a carrier you know with \
    --reference-hz";

fn kiwi_input(port: u16, freq_hz: f64) -> String {
    format!(
        "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {port}\nfreq_hz = {freq_hz:.1}\n"
    )
}

/// A fake KiwiSDR on a loopback port, and the flag that tells it to stop.
fn fake_kiwi() -> (
    u16,
    Arc<AtomicBool>,
    std::thread::JoinHandle<Result<(), String>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || common::fake_kiwi(listener, done)
    });
    (port, done, fake)
}

/// MAN-334 scenario 1: an offline receiver whose configured passband holds
/// no reference gets the retune hint, not a connection error.
#[test]
fn calibrate_gives_an_offline_receiver_the_retune_hint() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing listens on port 1.
    let cfg = write_cfg(dir.path(), &kiwi_input(1, 7_030_000.0));
    let out = calibrate(&cfg, &[]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(KIWI_7030_HINT), "{stderr}");
    assert!(!stderr.contains("TCP connect"), "{stderr}");
    assert!(!stderr.contains("Connection refused"), "{stderr}");
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
}

/// MAN-334 scenario 2: an online receiver whose configured passband holds no
/// reference is never connected to, so no session is held on it.
#[test]
fn calibrate_does_not_connect_to_a_receiver_whose_passband_holds_no_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (port, done, fake) = fake_kiwi();
    let cfg = write_cfg(dir.path(), &kiwi_input(port, 7_030_000.0));
    let out = calibrate(&cfg, &[]);
    done.store(true, Ordering::Relaxed);
    let session = fake.join().unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(KIWI_7030_HINT), "{stderr}");
    assert_eq!(
        session,
        Err("manta never connected to the fake KiwiSDR".to_string()),
        "calibrate opened the receiver before refusing its passband"
    );
}

/// MAN-334: the preflight does not refuse a passband that holds a reference;
/// the receiver is opened and measured on the plan of the opened source.
#[test]
fn calibrate_opens_a_receiver_whose_passband_holds_a_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (port, done, fake) = fake_kiwi();
    let cfg = write_cfg(dir.path(), &kiwi_input(port, 9_998_500.0));
    let mut child = common::KillOnDrop(
        manta()
            .args(["calibrate", "--duration", "12", "--config"])
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = common::line_channel(child.0.stderr.take().unwrap());
    common::expect_line(
        &stderr,
        &["measuring 10000000.0 Hz, time standard (WWV, WWVH, BPM) for 12 s"],
        Duration::from_secs(60),
    );
    drop(child);
    done.store(true, Ordering::Relaxed);
    assert_eq!(fake.join().unwrap(), Ok(()));
}

/// MAN-334: an HPSDR receiver's centre and rate are configured, so an offline
/// one also gets the retune hint before any C&C frame is sent.
#[test]
fn calibrate_gives_an_offline_hpsdr_receiver_the_retune_hint() {
    let out = manta()
        .args(["calibrate", "--duration", "12", "--hpsdr-host", "127.0.0.1"])
        .args(["--hpsdr-freq-hz", "7030000", "--hpsdr-rate-hz", "48000"])
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(
            "no known reference frequency is usable in this receiver's passband \
             (7006000.0 to 7054000.0 Hz)"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("--tune-hz 9998500"), "{stderr}");
    assert!(!stderr.contains("HPSDR"), "{stderr}");
}

/// MAN-334: a sound card's centre is the dial, so a dial far from any
/// reference gets the dial hint without the device being opened.
#[test]
fn calibrate_gives_a_sound_card_the_dial_hint_without_opening_it() {
    let out = manta()
        .args(["calibrate", "--duration", "12", "--device", "NoSuchCard"])
        .args(["--dial-freq-hz", "7030000"])
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(
            "no known reference frequency is usable in this receiver's passband \
             (7030300.0 to 7033000.0 Hz)"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("--dial-freq-hz 9998350"), "{stderr}");
    assert!(!stderr.contains("no audio input"), "{stderr}");
}

// ---- MAN-541: `calibrate --from-rbn` ----

/// One band row of a daemon's `rbn_compare` reading.
fn rbn_band(band: &str, hz: f64, n: u64, reading: Option<(f64, f64)>) -> serde_json::Value {
    serde_json::json!({
        "band": band, "median_offset_hz": hz, "samples": n,
        "median_ppm": reading.map(|r| r.0), "ppm_error": reading.map(|r| r.1),
    })
}

/// The design's four bands, nothing applied, suggestion -3.02 ±0.36.
fn design_entry(entry: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "entry": entry,
        "names": [entry.unwrap_or("file")],
        "applied_ppm": 0.0,
        "replaced_by_flag": false,
        "bands": [
            rbn_band("80m", 13.5, 41, Some((3.803, 2.2))),
            rbn_band("40m", 21.0, 8, None),
            rbn_band("20m", 42.0, 40, Some((2.995, 0.564))),
            rbn_band("15m", 63.0, 24, Some((2.996, 0.485))),
        ],
        "observed_ppm": 3.017,
        "ppm_error": 0.363,
        "suggested_ppm": -3.02
    })
}

/// A `/status` body; `rbn_inputs: None` is a daemon without comparison.
fn status_with(rbn_inputs: Option<Vec<serde_json::Value>>) -> String {
    let mut doc = serde_json::json!({
        "schema_version": 1, "version": "test", "uptime_seconds": 3600, "spots_total": 1,
        "telnet_clients": 0, "json_clients": 0, "ws_clients": 0, "active_tracks": 0,
        "uplink": {
            "health": "ok", "connected_targets": 0, "enabled_targets": 0, "sent_total": 0,
            "suppressed_total": 0, "reconnects_total": 0, "reconnect_window_seconds": 300,
            "flapping_threshold": 3, "targets": []
        }
    });
    if let Some(inputs) = rbn_inputs {
        doc["rbn_compare"] = serde_json::json!({
            "window_seconds": 3600, "min_samples": 20, "skew_warn_ppm": 2.0,
            "skew_hold_seconds": 1800, "bands": [], "inputs": inputs
        });
    }
    doc.to_string()
}

/// Runs `calibrate --from-rbn --addr <fake daemon>` plus `extra`, returning
/// the output and the daemon address.
fn from_rbn(
    body: &str,
    cfg: Option<&Path>,
    extra: &[&str],
    env: &[(&str, &str)],
) -> (Output, String) {
    let (addr, requests) = common::fake_server(common::http_reply("200 OK", body), 1);
    let mut cmd = manta();
    cmd.args(["calibrate", "--from-rbn", "--addr", &addr.to_string()]);
    if let Some(cfg) = cfg {
        cmd.arg("--config").arg(cfg);
    }
    cmd.args(extra);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    if let Ok(request) = requests.recv_timeout(Duration::from_secs(5)) {
        assert!(request.starts_with("GET /status HTTP/1.1"), "{request:?}");
    }
    (out, addr.to_string())
}

#[test]
fn calibrate_from_rbn_write_saves_the_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let (out, addr) = from_rbn(
        &status_with(Some(vec![design_entry(None)])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    let stdout = text(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        text(&out.stderr)
    );
    let want = format!(
        "source: RBN comparison of the daemon at {addr} over the last 3600 s, for [input]\n\
         evidence: 80m: +3.8 ±2.2 ppm, median +14 Hz over 41 spots\n\
         evidence: 40m: median +21 Hz over 8 spots, needs 20\n\
         evidence: 20m: +3.0 ±0.6 ppm, median +42 Hz over 40 spots\n\
         evidence: 15m: +3.0 ±0.5 ppm, median +63 Hz over 24 spots\n\
         measured: the receiver reads 3.02 ppm high against the RBN, with 0.00 ppm applied\n\
         correction: freq_correction_ppm = -3.02 (±0.36 ppm)\n\
         configured: 0.00 ppm, nothing set\n\
         saved: freq_correction_ppm = -3.02 in [input] of {} (was not set); restart manta to \
         apply it\n",
        cfg.display()
    );
    assert_eq!(stdout, want);
    let saved = std::fs::read_to_string(&cfg).unwrap();
    assert!(saved.contains("freq_correction_ppm = -3.02"), "{saved}");
    for line in FILE_INPUT.lines() {
        assert!(saved.lines().any(|l| l == line), "{line} lost: {saved}");
    }
}

#[test]
fn calibrate_from_rbn_replaces_the_applied_value() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        &format!("{FILE_INPUT}freq_correction_ppm = -2.0\n"),
    );
    let mut entry = design_entry(None);
    entry["applied_ppm"] = serde_json::json!(-2.0);
    entry["bands"] = serde_json::json!([rbn_band("20m", 14.0, 40, Some((1.0, 0.564)))]);
    entry["observed_ppm"] = serde_json::json!(1.0);
    entry["ppm_error"] = serde_json::json!(0.564);
    entry["suggested_ppm"] = serde_json::json!(-3.0);
    let (out, _) = from_rbn(
        &status_with(Some(vec![entry])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    let stdout = text(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        text(&out.stderr)
    );
    assert!(
        stdout.ends_with("(was -2.0); restart manta to apply it\n"),
        "{stdout}"
    );
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = -3.0\n"));
}

#[test]
fn calibrate_from_rbn_reports_already_set() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        &format!("{FILE_INPUT}freq_correction_ppm = -3.02\n"),
    );
    let (out, _) = from_rbn(
        &status_with(Some(vec![design_entry(None)])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.ends_with(&format!(
            "not saved: {} already has freq_correction_ppm = -3.02\n",
            cfg.display()
        )),
        "{stdout}"
    );
}

#[test]
fn calibrate_from_rbn_refuses_a_daemon_without_rbn_compare() {
    let (out, addr) = from_rbn(&status_with(None), None, &[], &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains(&format!(
            "the daemon at {addr} is not comparing its spots with the RBN; enable [rbn_compare]"
        )),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_from_rbn_refuses_without_a_reading() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let mut entry = design_entry(None);
    entry["bands"] = serde_json::json!([rbn_band("40m", 21.0, 8, None)]);
    entry["observed_ppm"] = serde_json::Value::Null;
    entry["ppm_error"] = serde_json::Value::Null;
    entry["suggested_ppm"] = serde_json::Value::Null;
    let (out, _) = from_rbn(
        &status_with(Some(vec![entry])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stdout).contains("evidence: 40m: median +21 Hz over 8 spots, needs 20"));
    assert!(
        text(&out.stderr).contains("no RBN reading for [input] yet"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}

#[test]
fn calibrate_from_rbn_refuses_a_coarse_reading() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let mut entry = design_entry(None);
    entry["ppm_error"] = serde_json::json!(1.3);
    let (out, _) = from_rbn(
        &status_with(Some(vec![entry])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stdout).contains("correction: freq_correction_ppm = -3.02 (±1.30 ppm)"));
    assert!(
        text(&out.stderr).contains("too coarse to save (needs ±1.00 or better)"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}

#[test]
fn calibrate_from_rbn_write_refuses_a_daemon_source_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let mut entry = design_entry(None);
    entry["replaced_by_flag"] = serde_json::json!(true);
    let body = status_with(Some(vec![entry]));
    let (out, _) = from_rbn(&body, Some(&cfg), &["--write"], &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains("--write refuses to save the RBN reading of a daemon"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
    // Without --write: the report, then why it is not saved.
    let (out, _) = from_rbn(&body, Some(&cfg), &[], &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(text(&out.stdout).ends_with(&format!(
        "not saved: the daemon's receiver came from a command-line source flag, not from {}\n",
        cfg.display()
    )));
}

const TWO_ENTRIES: &str = "[[input]]\nname = \"kiwi\"\ntype = \"file\"\npath = \"ref.wav\"\n\
                           iq = true\n\n[[input]]\nname = \"hl2\"\ntype = \"file\"\n\
                           path = \"ref.wav\"\niq = true\nfreq_correction_ppm = 1.5\n";

#[test]
fn calibrate_from_rbn_saves_into_the_named_entry() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), TWO_ENTRIES);
    let mut hl2 = design_entry(Some("hl2"));
    hl2["applied_ppm"] = serde_json::json!(1.5);
    hl2["suggested_ppm"] = serde_json::json!(-0.4);
    let body = status_with(Some(vec![design_entry(Some("kiwi")), hl2]));
    let (out, _) = from_rbn(&body, Some(&cfg), &["--input", "hl2", "--write"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        TWO_ENTRIES.replace("freq_correction_ppm = 1.5", "freq_correction_ppm = -0.4")
    );
}

#[test]
fn calibrate_from_rbn_needs_input_with_several_entries() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), TWO_ENTRIES);
    let mut cmd = manta();
    let out = cmd
        .args([
            "calibrate",
            "--from-rbn",
            "--addr",
            "127.0.0.1:1",
            "--config",
        ])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("--input"), "{stderr}");
}

#[test]
fn calibrate_from_rbn_refuses_an_entry_the_daemon_does_not_report() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), TWO_ENTRIES);
    let body = status_with(Some(vec![design_entry(None)]));
    let (out, addr) = from_rbn(&body, Some(&cfg), &["--input", "hl2"], &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains(&format!(
            "the daemon at {addr} reports no RBN reading for the [[input]] entry \"hl2\"; it \
             reports [input]; point --addr at the daemon that runs {}",
            cfg.display()
        )),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn calibrate_from_rbn_json_reports_the_reading() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let (out, addr) = from_rbn(
        &status_with(Some(vec![design_entry(None)])),
        Some(&cfg),
        &["--json"],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["source"], "rbn");
    assert_eq!(v["daemon"], addr.as_str());
    assert_eq!(v["window_seconds"], 3600);
    assert_eq!(v["input"], serde_json::Value::Null);
    assert_eq!(v["observed_ppm"], 3.017);
    assert_eq!(v["result"]["freq_correction_ppm"], -3.02);
    assert_eq!(v["bands"].as_array().unwrap().len(), 4);
    assert_eq!(v["written"], false);
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}

#[test]
fn calibrate_from_rbn_opens_no_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let kiwi = TcpListener::bind("127.0.0.1:0").unwrap();
    kiwi.set_nonblocking(true).unwrap();
    let port = kiwi.local_addr().unwrap().port();
    let cfg = write_cfg(dir.path(), &kiwi_input(port, 9_998_500.0));
    let (out, _) = from_rbn(
        &status_with(Some(vec![design_entry(None)])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        matches!(kiwi.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "calibrate --from-rbn connected to the receiver"
    );
}

#[test]
fn calibrate_from_rbn_conflicts_with_measurement_flags() {
    for extra in [
        &["--duration", "60"][..],
        &["--source", "x.wav"],
        &["--tune-hz", "9998500"],
    ] {
        let out = manta()
            .args(["calibrate", "--from-rbn"])
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{extra:?}: {out:?}");
        assert!(
            text(&out.stderr).contains("cannot be used with"),
            "{extra:?}: {}",
            text(&out.stderr)
        );
    }
    let out = manta()
        .args(["calibrate", "--addr", "127.0.0.1:7302"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "--addr needs --from-rbn: {out:?}"
    );
}

#[test]
fn calibrate_from_rbn_warns_when_the_environment_overrides_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let (out, _) = from_rbn(
        &status_with(Some(vec![design_entry(None)])),
        Some(&cfg),
        &["--write"],
        &[("MANTA_INPUT_FREQ_CORRECTION_PPM", "1.0")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(&format!(
            "warning: MANTA_INPUT_FREQ_CORRECTION_PPM is set and overrides freq_correction_ppm \
             from {} for run, soak and doctor; unset it to use the saved value",
            cfg.display()
        )),
        "{}",
        text(&out.stderr)
    );
}

/// MAN-541 review: a lone `[[input]]` entry needs no `--input`. The loader
/// folds it into `[input]` (MAN-393), so the daemon reports it with
/// `entry: null`, and calibrate matches it the same way.
#[test]
fn calibrate_from_rbn_saves_a_lone_input_entry_without_input() {
    let dir = tempfile::tempdir().unwrap();
    let original = "[[input]]\nname = \"kiwi\"\ntype = \"file\"\npath = \"ref.wav\"\niq = true\n";
    let cfg = write_cfg(dir.path(), original);
    let body = status_with(Some(vec![design_entry(None)]));
    let (out, _) = from_rbn(&body, Some(&cfg), &["--write"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(std::fs::read_to_string(&cfg)
        .unwrap()
        .contains("freq_correction_ppm = -3.02"));
}

/// MAN-541 review: a peer's out-of-range suggestion is never saved.
#[test]
fn calibrate_from_rbn_refuses_an_out_of_range_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), FILE_INPUT);
    let mut entry = design_entry(None);
    entry["suggested_ppm"] = serde_json::json!(1e300);
    let (out, _) = from_rbn(
        &status_with(Some(vec![entry])),
        Some(&cfg),
        &["--write"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains("outside the supported range"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), FILE_INPUT);
}
