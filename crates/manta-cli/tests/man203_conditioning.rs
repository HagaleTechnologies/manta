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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
