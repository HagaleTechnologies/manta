//! MAN-496 acceptance: the operator surface for the MAN-203 conditioning
//! chain, through the real binary and config file. The recheck's residual
//! finding was that `PipelineConfig::conditioning` was set only in tests:
//! nothing an operator could reach turned a stage on.
//!
//!   Scenario: `dc_block = "apply"` in `[input]` makes decode spot the
//!             near-centre station the DC offset hid, and `inputConditioning`
//!             appears in `decode --json` only when a stage is on.
//!   Scenario: A bad mode value is refused by `manta config check` with the
//!             exact key error.

#[path = "../../manta-engine/tests/support/man203.rs"]
mod support;

use manta_testkit::sensitivity::{CENTER_FREQ_HZ, SAMPLE_RATE_HZ};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::near_centre_scene;

/// The scene MAN-203 measured (docs/DECISIONS/2026-11-03): one 6 dB
/// station 60 Hz from centre under a `(0.01, 0.007)` DC offset; bypass
/// spots nothing, `dc_block = "apply"` spots the call.
const OFFSET_HZ: f64 = 60.0;
const SNR_DB: f64 = 6.0;
const DC: f32 = 0.01;

fn manta(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(args)
        .output()
        .expect("spawn manta")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Writes the near-centre DC scene as a fixture WAV, and two configs beside
/// it: one bare `[input]` (conditioning off), one with `dc_block = "apply"`.
fn fixtures(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let scene = near_centre_scene(OFFSET_HZ, SNR_DB, DC, 1);
    let wav = manta_testkit::wav::write_fixture(
        dir,
        "near_centre_dc",
        &scene.samples,
        SAMPLE_RATE_HZ,
        CENTER_FREQ_HZ,
    )
    .unwrap();
    let off = dir.join("off.toml");
    std::fs::write(&off, "[input]\ntype = \"audio\"\n").unwrap();
    let on = dir.join("dc_apply.toml");
    std::fs::write(&on, "[input]\ntype = \"audio\"\ndc_block = \"apply\"\n").unwrap();
    (wav, off, on)
}

/// `decode --json` stdout parsed: `Ok` when the decode reported, `Err`
/// with its stderr when it found nothing (an all-off decode of the
/// DC-offset scene may legitimately find no signal).
fn decode_json(cfg: &Path, wav: &Path) -> std::result::Result<serde_json::Value, String> {
    let out = manta(&[
        "decode",
        "--json",
        "--config",
        &cfg.display().to_string(),
        &wav.display().to_string(),
    ]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(!stderr.contains("panicked"), "stderr:\n{stderr}");
    if !out.status.success() {
        return Err(stderr);
    }
    Ok(serde_json::from_str(stdout.trim()).unwrap())
}

fn spotted_calls(report: &serde_json::Value) -> Vec<String> {
    report["spots"]
        .as_array()
        .map(|spots| {
            spots
                .iter()
                .map(|s| s["dxCall"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn decode_with_dc_block_apply_spots_the_near_centre_station() {
    let tmp = tempfile::tempdir().unwrap();
    let (wav, off, on) = fixtures(tmp.path());
    let scene = near_centre_scene(OFFSET_HZ, SNR_DB, DC, 1);
    let call = &scene.stations[0].call;

    let bare = manta(&[
        "decode",
        "--config",
        &off.display().to_string(),
        &wav.display().to_string(),
    ]);
    if bare.status.success() {
        let found = spotted_calls(
            &serde_json::from_str::<serde_json::Value>(text(&bare.stdout).trim()).unwrap(),
        );
        assert!(
            !found.contains(call),
            "bypass spotted {call} through the DC offset: {found:?}"
        );
    } else {
        assert!(
            text(&bare.stderr).contains("no signal found"),
            "bare decode failed unexpectedly: {}",
            text(&bare.stderr)
        );
    }

    let report = decode_json(&on, &wav).expect("dc_block = \"apply\" decodes the scene");
    let found = spotted_calls(&report);
    assert!(
        found.contains(call),
        "dc_block = \"apply\" did not spot {call}: {found:?}"
    );
}

#[test]
fn decode_json_input_conditioning_matches_the_metric_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let (wav, off, on) = fixtures(tmp.path());

    let off_report = decode_json(&off, &wav);
    if let Ok(off_report) = &off_report {
        assert!(
            off_report.get("inputConditioning").is_none(),
            "an all-off config must keep the pre-MAN-496 JSON shape: {off_report}"
        );
    }

    let on_report = decode_json(&on, &wav).expect("dc_block = \"apply\" decodes the scene");
    let cond = on_report
        .get("inputConditioning")
        .expect("dc_block = \"apply\" reports inputConditioning");
    assert_eq!(cond["dc"]["mode"], "apply");
    assert_eq!(cond["impulse"]["mode"], "off");
    assert!(
        cond["dc"]["correctedSamples"].as_u64().unwrap_or(0) > 0,
        "the apply DC stage corrects samples: {cond}"
    );
    assert!(cond["dc"]["levelDbfs"].is_number());
    assert!(cond["spans"].as_array().is_some_and(|s| !s.is_empty()));
    for key in [
        "flaggedSamples",
        "repairedSamples",
        "longRuns",
        "pausedSamples",
    ] {
        assert!(cond["impulse"].get(key).is_some(), "{key} missing: {cond}");
    }
    assert!(cond.get("spansDropped").is_some());
}

#[test]
fn bad_conditioning_mode_is_refused_by_config_check() {
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.toml");
    std::fs::write(&bad, "[input]\ntype = \"audio\"\ndc_block = \"on\"\n").unwrap();
    let out = manta(&["config", "check", "--config", &bad.display().to_string()]);
    assert!(
        !out.status.success(),
        "dc_block = \"on\" passed config check"
    );
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("[input] dc_block must be \"off\", \"observe\" or \"apply\", not \"on\""),
        "stderr:\n{stderr}"
    );
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

#[test]
fn run_daemon_publishes_the_conditioned_samples_metric_from_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let (wav, _, _) = fixtures(tmp.path());
    for multi in [false, true] {
        let [telnet_port, json_port, metrics_port] = free_ports();
        let cfg = tmp.path().join("daemon.toml");
        let mut inputs = format!("[{}]\n{}type = \"file\"\npath = {:?}\niq = true\nrealtime = true\nloop = true\ndc_block = \"apply\"\n",
            if multi { "[input]" } else { "input" },
            if multi { "name = \"rx1\"\n" } else { "" }, wav);
        if multi {
            inputs.push_str(&format!("[[input]]\nname = \"rx2\"\ntype = \"file\"\npath = {:?}\niq = true\nrealtime = true\nloop = true\n", wav));
        }
        std::fs::write(&cfg, format!("{inputs}\n[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\njson_port = {json_port}\nmetrics_port = {metrics_port}\n")).unwrap();
        let log = tmp.path().join("daemon.stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--config"])
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&log).unwrap()))
            .spawn()
            .unwrap();
        let _guard = ChildGuard(child);
        let source = if multi { "rx1" } else { "file" };
        let metric = format!("manta_input_conditioned_samples_total{{source=\"{source}\",stage=\"dc\",mark=\"corrected\"}}");
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut seen = None;
        while Instant::now() < deadline {
            if let Some(body) = http_get(metrics_port, "/metrics") {
                assert!(
                    body.contains("# TYPE manta_input_conditioned_samples_total counter"),
                    "{body}"
                );
                if multi {
                    assert!(
                        !body.contains("manta_input_conditioned_samples_total{source=\"rx2\""),
                        "off input must have no conditioning series"
                    );
                }
                if let Some(n) = metric_value(&body, &metric) {
                    if seen.is_none() {
                        assert_eq!(n, 0.0, "configured stage starts at zero");
                    }
                    seen = Some(n);
                    if n > 0.0 {
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            seen.is_some_and(|n| n > 0.0),
            "{metric}: {seen:?}; {}",
            std::fs::read_to_string(&log).unwrap()
        );
    }
}
