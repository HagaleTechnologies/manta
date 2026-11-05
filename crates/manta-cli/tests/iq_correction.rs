//! MAN-494: exercise calibration through config-driven production paths.
#[path = "../../manta-engine/tests/support/man203.rs"]
mod support;

use manta_dsp::condition::iq::{IqCalibration, ToneMeter, ToneResult};
use num_complex::Complex32;
use std::path::Path;
use std::process::{Command, Output};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}
fn write_wav(path: &Path, samples: &[Complex32]) {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 2,
            sample_rate: 96000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for s in samples {
        writer.write_sample(s.re).unwrap();
        writer.write_sample(s.im).unwrap();
    }
    writer.finalize().unwrap();
    std::fs::write(
        path.with_extension("json"),
        r#"{"center_freq_hz":14000000.0}"#,
    )
    .unwrap();
}
fn measured_for(gain: f64, phase: f64, center: f64) -> IqCalibration {
    let mut meter = ToneMeter::new(96000.0, 12000.0);
    for sample in support::iq_tone_capture(gain, phase) {
        meter.push(sample);
    }
    let ToneResult::Accepted { rho, .. } = meter.result() else {
        panic!("calibration rejected")
    };
    IqCalibration::from_image_ratio(rho, center).unwrap()
}
fn settings(mode: &str, c: IqCalibration) -> String {
    format!(
        "iq_correction = {mode:?}\niq_gain_ratio = {:.17}\niq_phase_deg = {:.17}\niq_calibrated_center_hz = {:.1}\n",
        c.gain_ratio, c.phase_deg, c.center_freq_hz
    )
}
fn decode(path: &Path, config: &Path) -> (serde_json::Value, Output) {
    let out = manta()
        .arg("decode")
        .arg(path)
        .arg("--json")
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), out)
}
fn outcome(report: &serde_json::Value) -> support::Outcome {
    support::Outcome {
        spots: report["spots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["dxCall"].as_str().unwrap().to_string(),
                    s["frequency"].as_f64().unwrap(),
                )
            })
            .collect(),
    }
}
#[test]
fn config_decode_corrects_a_measured_image_and_preserves_bypass_modes() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("impaired.wav");
    let config = dir.path().join("manta.toml");
    let c = measured_for(1.05, 2.0, 14000000.0);
    let clean = support::iq_scene(1);
    let want = support::found_calls(
        &clean,
        &support::decode(&clean.samples, &Default::default()),
    );
    let mut scene = support::iq_scene(1);
    support::add_iq_imbalance(&mut scene.samples, 1.05, 2.0);
    write_wav(&wav, &scene.samples);
    std::fs::write(&config, "").unwrap();
    let (bypass, _) = decode(&wav, &config);
    assert!(
        support::mirror_spots(&scene, &outcome(&bypass)) > 0,
        "bypass must reproduce a mirror spot"
    );
    std::fs::write(&config, format!("[input]\n{}", settings("apply", c))).unwrap();
    let (applied, output) = decode(&wav, &config);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("IQ correction withheld"));
    assert_eq!(support::mirror_spots(&scene, &outcome(&applied)), 0);
    assert_eq!(support::found_calls(&scene, &outcome(&applied)), want);
    std::fs::write(&config, "[input]\niq_correction = \"observe\"\n").unwrap();
    let (observe, _) = decode(&wav, &config);
    assert_eq!(observe, bypass);
    let mismatch = IqCalibration {
        center_freq_hz: 14030000.0,
        ..c
    };
    std::fs::write(&config, format!("[input]\n{}", settings("apply", mismatch))).unwrap();
    let (withheld, output) = decode(&wav, &config);
    assert_eq!(withheld, bypass);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("IQ correction withheld").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("14030.0 kHz, receiver at 14000.0 kHz"),
        "{stderr}"
    );
}

#[test]
fn decode_rejects_conflicting_multi_input_conditioning() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("manta.toml");
    std::fs::write(&config, "[[input]]\nname = \"a\"\ntype = \"audio\"\ncenter_freq_hz = 14000000.0\niq_correction = \"observe\"\n[[input]]\nname = \"b\"\ntype = \"audio\"\ncenter_freq_hz = 14000000.0\n").unwrap();
    let out = manta()
        .arg("decode")
        .arg("nonexistent.wav")
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("conflicting IQ conditioning"));
}

#[test]
fn replay_inputs_keep_different_calibrations_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("multi.toml");
    let a = dir.path().join("a.wav");
    let b = dir.path().join("b.wav");
    let ca = measured_for(1.05, 2.0, 14000000.0);
    let cb = measured_for(0.95, -3.0, 7000000.0);
    let mut scenes = [support::iq_scene(1), support::iq_scene(2)];
    support::add_iq_imbalance(&mut scenes[0].samples, 1.05, 2.0);
    support::add_iq_imbalance(&mut scenes[1].samples, 0.95, -3.0);
    write_wav(&a, &scenes[0].samples);
    write_wav(&b, &scenes[1].samples);
    std::fs::write(b.with_extension("json"), r#"{"center_freq_hz":7000000.0}"#).unwrap();
    let entries = |mode| {
        format!(
            "[[input]]\nname = \"a\"\ntype = \"file\"\npath = {}\niq = true\n{}\n[[input]]\nname = \"b\"\ntype = \"file\"\npath = {}\niq = true\n{}",
            serde_json::to_string(&a).unwrap(),
            settings(mode, ca),
            serde_json::to_string(&b).unwrap(),
            settings(mode, cb)
        )
    };
    let run = || {
        let out = manta()
            .args(["run", "--json", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let spots = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        support::Outcome {
            spots: spots
                .iter()
                .filter_map(|s| s.get("spot"))
                .map(|s| {
                    (
                        s["dxCall"].as_str().unwrap().to_string(),
                        s["frequency"].as_f64().unwrap(),
                    )
                })
                .collect(),
        }
    };
    std::fs::write(&config, entries("off")).unwrap();
    let bypass = run();
    let at_original_center = |out: &support::Outcome, index: usize| support::Outcome {
        spots: out
            .spots
            .iter()
            .map(|(call, freq)| (call.clone(), freq + index as f64 * 7000000.0))
            .collect(),
    };
    for (index, scene) in scenes.iter().enumerate() {
        let out = at_original_center(&bypass, index);
        assert!(support::mirror_spots(scene, &out) > 0);
    }
    std::fs::write(&config, entries("apply")).unwrap();
    let corrected = run();
    for (index, scene) in scenes.iter().enumerate() {
        let out = at_original_center(&corrected, index);
        assert_eq!(support::mirror_spots(scene, &out), 0);
        assert_eq!(
            support::found_calls(scene, &out).len(),
            scene.stations.len()
        );
    }
}
