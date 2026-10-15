//! MAN-234 acceptance: one bad input sample (NaN, ±Inf, or a finite value
//! beyond `MAX_SAMPLE_MAGNITUDE = 1e17`) is replaced with silence and
//! counted instead of ending the run or deafening it.
//!
//!   Scenario: A not-a-number sample during an active track does not crash
//!   Scenario: An infinite sample mid-file does not stop decoding
//!   Scenario: A bad sample before any track exists does not deafen the run
//!
//! Measured on the base commit (`a613c8c`) with these 20 s V1 fixtures:
//! NaN/+Inf at 10 s ended `decode`, `run --json` (after 35 lines) and
//! `doctor` with exit 101 at `track.rs:1030`; NaN/+Inf at 1 ms with exit
//! 101 at `floor.rs:130`; a finite `f32::MAX` at 1 ms made `decode` fail
//! `no signal found`, `run --json` exit 0 with no output and `doctor`
//! report `NO_SIGNAL`. Clean: `decode` exit 0, `run --json` 75 lines,
//! `doctor` `WEAK_NO_DECODE`.

use manta_testkit::vectors::{self, VectorSpec};
use num_complex::Complex32;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Sample index of the mid-track fixtures' bad sample (t = 10 s at 96 kS/s).
const MID: usize = 960_000;
/// Sample index of the start fixtures' bad sample (t = 1 ms).
const START: usize = 96;
/// Lines that differ from clean must have `sample_ts` in
/// `[MID, MID + WINDOW]`: 5 s after the bad sample.
const WINDOW: u64 = 5 * 96_000;

struct Rendered {
    spec: VectorSpec,
    samples: Vec<Complex32>,
}

fn rendered() -> &'static Rendered {
    static R: OnceLock<Rendered> = OnceLock::new();
    R.get_or_init(|| {
        let spec = VectorSpec {
            duration_s: 20.0,
            ..vectors::v1()
        };
        let samples = vectors::render(&spec).unwrap().samples;
        Rendered { spec, samples }
    })
}

/// The six fixtures: name, and the bad `re` value at which index (`None`
/// for clean).
const FIXTURES: [(&str, Option<(usize, f32)>); 6] = [
    ("clean", None),
    ("nan_mid", Some((MID, f32::NAN))),
    ("inf_mid", Some((MID, f32::INFINITY))),
    ("nan_start", Some((START, f32::NAN))),
    ("inf_start", Some((START, f32::INFINITY))),
    ("max_start", Some((START, f32::MAX))),
];

/// Writes every fixture as a float32 IQ WAV plus sidecar into `dir`.
fn write_fixtures(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    let r = rendered();
    FIXTURES
        .iter()
        .map(|&(name, bad)| {
            let mut samples = r.samples.clone();
            if let Some((idx, value)) = bad {
                samples[idx].re = value;
            }
            let path = manta_testkit::wav::write_fixture(
                dir,
                name,
                &samples,
                r.spec.fs,
                r.spec.center_freq_hz,
            )
            .unwrap();
            (name, path)
        })
        .collect()
}

fn manta(args: &[&str], wav: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(args)
        .arg(wav)
        .output()
        .expect("spawn manta")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Asserts a clean exit with no panic, returning (stdout, stderr).
fn ok(name: &str, out: &Output) -> (String, String) {
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(
        out.status.success() && !stderr.contains("panicked"),
        "{name}: exit {:?}\nstderr:\n{stderr}",
        out.status.code()
    );
    (stdout, stderr)
}

const ONE_BAD: &str = "1 bad sample (NaN, infinite, or larger than 1e17 in magnitude)";

/// The `(event, track_id, sample_ts, glyph)` identity of one `run --json`
/// or `decode --json` event, plus its `sample_ts` when it has one.
fn identity(v: &serde_json::Value) -> (String, Option<u64>, Option<u64>, Option<String>) {
    // Decoder events are tagged `{"event":"TrackMeta",...}`; a spot line is
    // `{"spot":{...}}`.
    let (event, body) = match (v.get("event").and_then(|e| e.as_str()), v.get("spot")) {
        (Some(e), _) => (e.to_string(), v),
        (None, Some(spot)) => ("spot".to_string(), spot),
        (None, None) => (String::new(), v),
    };
    (
        event,
        body.get("track_id").and_then(|x| x.as_u64()),
        body.get("sample_ts").and_then(|x| x.as_u64()),
        body.get("glyph").map(|g| g.to_string()),
    )
}

fn sample_ts(v: &serde_json::Value) -> Option<u64> {
    identity(v).2
}

fn in_window(v: &serde_json::Value) -> bool {
    sample_ts(v).is_some_and(|ts| (MID as u64..=MID as u64 + WINDOW).contains(&ts))
}

#[test]
fn decode_keeps_copying_through_one_bad_sample() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());
    let (clean_out, _) = ok("clean", &manta(&["decode"], &fixtures[0].1));
    assert!(!clean_out.trim().is_empty());
    for (name, path) in &fixtures[1..] {
        let (stdout, stderr) = ok(name, &manta(&["decode"], path));
        assert_eq!(stdout, clean_out, "{name}: decoded text differs from clean");
        assert!(
            stderr.contains(&format!("contained {ONE_BAD}")),
            "{name}: stderr:\n{stderr}"
        );
    }
}

#[test]
fn decode_json_reports_bad_samples_only_when_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());
    let json = |name: &str, path: &Path| -> serde_json::Value {
        let (stdout, _) = ok(name, &manta(&["decode", "--json"], path));
        serde_json::from_str(stdout.trim()).unwrap()
    };
    let clean = json("clean", &fixtures[0].1);
    assert!(clean.get("bad_samples").is_none(), "{clean}");
    let nan = json("nan_mid", &fixtures[1].1);
    assert_eq!(nan["bad_samples"], 1);

    let (clean_obj, nan_obj) = (clean.as_object().unwrap(), nan.as_object().unwrap());
    for (key, want) in clean_obj {
        if key == "events" {
            continue;
        }
        assert_eq!(&nan_obj[key], want, "key {key}");
    }
    let (want, got) = (
        clean["events"].as_array().unwrap(),
        nan["events"].as_array().unwrap(),
    );
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want) {
        assert_eq!(identity(g), identity(w));
        assert!(
            g == w || in_window(w),
            "differs outside the window: {g} vs {w}"
        );
    }
}

#[test]
fn listen_matches_clean_outside_a_bounded_window() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());
    let run = |name: &str, path: &Path| -> (String, String) {
        ok(
            name,
            &manta(&["run", "--source-iq", "--json", "--source"], path),
        )
    };
    let (clean_out, _) = run("clean", &fixtures[0].1);
    let clean_lines: Vec<&str> = clean_out.lines().collect();
    assert!(!clean_lines.is_empty());
    for (name, path) in &fixtures[1..] {
        let (stdout, stderr) = run(name, path);
        assert!(
            stderr.contains(&format!("delivered {ONE_BAD}")),
            "{name}: stderr:\n{stderr}"
        );
        if name.ends_with("_start") {
            assert_eq!(stdout, clean_out, "{name}: run --json differs from clean");
            continue;
        }
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), clean_lines.len(), "{name}: line count");
        for (got, want) in lines.iter().zip(&clean_lines) {
            if got == want {
                continue;
            }
            let (g, w): (serde_json::Value, serde_json::Value) = (
                serde_json::from_str(got).unwrap(),
                serde_json::from_str(want).unwrap(),
            );
            assert!(w.get("spot").is_none(), "{name}: spot line differs: {got}");
            assert_eq!(identity(&g), identity(&w), "{name}");
            assert!(
                in_window(&w),
                "{name}: line differs outside the window: {got}"
            );
        }
    }
}

#[test]
fn doctor_verdict_survives_a_bad_sample_before_any_track() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());
    let doctor = |name: &str, path: &Path, json: bool| -> String {
        let mut args = vec!["doctor", "--source-iq", "--duration", "60"];
        if json {
            args.push("--json");
        }
        args.push("--source");
        ok(name, &manta(&args, path)).0
    };
    let verdict = |report: &str| -> String {
        report
            .lines()
            .find(|l| l.starts_with("verdict:"))
            .unwrap_or_default()
            .to_string()
    };
    let clean = doctor("clean", &fixtures[0].1, false);
    let clean_verdict = verdict(&clean);
    assert!(!clean_verdict.is_empty(), "{clean}");
    assert!(!clean.contains("input:"), "{clean}");
    let clean_json: serde_json::Value =
        serde_json::from_str(doctor("clean", &fixtures[0].1, true).trim()).unwrap();
    assert!(clean_json.get("bad_samples").is_none(), "{clean_json}");
    for (name, path) in &fixtures[1..] {
        let report = doctor(name, path, false);
        assert_eq!(verdict(&report), clean_verdict, "{name}:\n{report}");
        assert!(!report.contains("NO_SIGNAL"), "{name}:\n{report}");
        assert!(
            report.contains(&format!("input: {ONE_BAD} replaced with silence")),
            "{name}:\n{report}"
        );
        let json: serde_json::Value =
            serde_json::from_str(doctor(name, path, true).trim()).unwrap();
        assert_eq!(json["bad_samples"], 1, "{name}: {json}");
    }
}

#[test]
fn check_still_rejects_nonfinite_input() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());
    let out = manta(&["check", "--source-iq"], &fixtures[3].1);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("nonfinite input sample"), "{stderr}");
}

/// `N` distinct ports nothing listens on right now. All `N` listeners are
/// held until every port is read (MAN-371), as `node_health_acceptance.rs`
/// does.
fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}

/// Kills and reaps the daemon child on drop, including on a failing assert.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `GET <path>` over a raw TCP connection; returns the body.
fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .ok()?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp).ok()?;
    resp.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
}

/// The value trailing a Prometheus sample line starting with
/// `metric_with_labels`.
fn metric_value(body: &str, metric_with_labels: &str) -> Option<f64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

const BAD_METRIC: &str = r#"manta_input_bad_samples_total{source="file"}"#;

/// Starts `manta run --loop` on `wav` with a `[server]` table, and scrapes
/// `/metrics` until `done(value)` holds or 20 s pass. Returns the last
/// value seen and the child's stderr.
fn daemon_bad_samples(dir: &Path, wav: &Path, done: impl Fn(f64) -> bool) -> (f64, String) {
    let [telnet_port, json_port, metrics_port] = free_ports();
    let cfg_path = dir.join(format!(
        "{}.toml",
        wav.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &cfg_path,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             telnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\n"
        ),
    )
    .unwrap();
    let stderr_path = dir.join(format!(
        "{}.stderr.log",
        wav.file_stem().unwrap().to_string_lossy()
    ));
    let child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["run", "--source-iq", "--loop", "--source"])
        .arg(wav)
        .arg("--config")
        .arg(&cfg_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(&stderr_path).unwrap()))
        .spawn()
        .expect("spawn manta run");
    let _guard = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = None;
    loop {
        if let Some(v) =
            http_get(metrics_port, "/metrics").and_then(|b| metric_value(&b, BAD_METRIC))
        {
            last = Some(v);
            if done(v) {
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give the first-sample warning line time to reach the log file.
    std::thread::sleep(Duration::from_millis(200));
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let value = last.unwrap_or_else(|| panic!("{BAD_METRIC} never scraped; stderr:\n{stderr}"));
    (value, stderr)
}

#[test]
fn daemon_publishes_bad_samples_from_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let fixtures = write_fixtures(tmp.path());

    let (clean, stderr) = daemon_bad_samples(tmp.path(), &fixtures[0].1, |_| true);
    assert_eq!(clean, 0.0, "first scrape on clean input; stderr:\n{stderr}");

    let (bad, stderr) = daemon_bad_samples(tmp.path(), &fixtures[3].1, |v| v >= 1.0);
    assert!(bad >= 1.0, "{BAD_METRIC} = {bad}; stderr:\n{stderr}");
    assert!(
        stderr.contains("counted in manta_input_bad_samples_total"),
        "stderr:\n{stderr}"
    );
}
