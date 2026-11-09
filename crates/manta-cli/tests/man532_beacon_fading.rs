//! MAN-532: full historical-duration trailing-T beacon safety coverage.
//! Watterson uses coppa f8a4d16d; SNR is referenced to 2500 Hz.
//! EOF completes track closure. Test every output spot, regardless of type.

use std::process::Command;
use std::sync::OnceLock;

use coppa_channel::watterson::WattersonPreset;
use manta_testkit::scene::{SignalSpec, WattersonFade};
use manta_testkit::vectors::VectorSpec;

fn spec(snr_db: f32, watterson: Option<WattersonFade>) -> VectorSpec {
    VectorSpec {
        name: if watterson.is_some() {
            "man532-beacon-fade"
        } else {
            "man532-beacon-awgn"
        },
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x4D41_4E31_3230, // "MAN120"
        signals: vec![SignalSpec {
            dit_units: 1.0,
            element_gap_units: 1.0,
            text: "K5ARH T".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 9_100.0,
            snr_2500_db: snr_db,
            jitter: None,
            qsb: None,
            watterson,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
    }
}

// Process-local fixtures cannot reuse the old 40-second MAN-120 cache.
// Each directory identifies SNR and channel; all other parameters are fixed
// above. OnceLock shares each rendered scene across allowlist variants.
fn fixtures() -> &'static tempfile::TempDir {
    static FIXTURES: OnceLock<tempfile::TempDir> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let root = tempfile::Builder::new()
            .prefix("man532-120s-")
            .tempdir()
            .unwrap();
        for snr in [6, 10] {
            for faded in [false, true] {
                let dir = root.path().join(format!("snr{snr}-poor{faded}"));
                std::fs::create_dir_all(&dir).unwrap();
                let fade = faded.then_some(WattersonFade {
                    preset: WattersonPreset::Poor,
                    seed: 0x5631,
                });
                let scene = spec(snr as f32, fade);
                manta_testkit::vectors::write_fixture_set(&scene, &dir).unwrap();
                assert!(dir.join(format!("{}.wav", scene.name)).is_file());
                assert!(dir.join(format!("{}.manifest.json", scene.name)).is_file());
            }
        }
        root
    })
}

fn decode(snr: u32, faded: bool, listed: bool) -> Vec<(String, String, f64)> {
    let name = if faded {
        "man532-beacon-fade"
    } else {
        "man532-beacon-awgn"
    };
    let wav = fixtures()
        .path()
        .join(format!("snr{snr}-poor{faded}"))
        .join(format!("{name}.wav"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    cmd.args(["decode", "--json", "--engine", "legacy"]);
    if listed {
        cmd.args(["--allowlist", "K5ARH"]);
    }
    let out = cmd.arg(&wav).output().unwrap();
    assert!(
        out.status.success(),
        "SNR={snr}, faded={faded}, listed={listed}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = spots(&serde_json::from_slice(&out.stdout).unwrap());
    eprintln!("SNR={snr}, faded={faded}, listed={listed}: {got:?}");
    got
}

/// `(dxCall, spotType, sampleTs in seconds)` for every spot.
fn spots(report: &serde_json::Value) -> Vec<(String, String, f64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["dxCall"].as_str().unwrap().to_string(),
                s["spotType"].as_str().unwrap().to_string(),
                s["sampleTs"].as_f64().unwrap() / 96_000.0,
            )
        })
        .collect()
}

fn check_calls(got: &[(String, String, f64)], snr: u32, faded: bool, listed: bool) {
    assert!(
        got.iter().all(|(call, _, _)| call == "K5ARH"),
        "SNR={snr}, faded={faded}, listed={listed}: bogus call in {got:?}"
    );
}

#[test]
fn faded_unlisted_trailing_t_emits_no_beacon_or_bogus_call() {
    for snr in [6, 10] {
        let got = decode(snr, true, false);
        check_calls(&got, snr, true, false);
        assert!(
            got.iter().all(|(_, ty, _)| ty != "beacon"),
            "SNR={snr}, listed=false: {got:?}"
        );
    }
}

#[test]
fn faded_allowlisted_beacon_emits_only_the_scene_call() {
    for snr in [6, 10] {
        let got = decode(snr, true, true);
        check_calls(&got, snr, true, true);
        assert!(
            got.iter()
                .any(|(call, ty, _)| call == "K5ARH" && ty == "beacon"),
            "SNR={snr}, listed=true: {got:?}"
        );
    }
}

#[test]
fn awgn_unlisted_trailing_t_emits_no_beacon_or_bogus_call() {
    for snr in [6, 10] {
        let got = decode(snr, false, false);
        check_calls(&got, snr, false, false);
        assert!(
            got.iter().all(|(_, ty, _)| ty != "beacon"),
            "SNR={snr}, listed=false: {got:?}"
        );
    }
}

#[test]
fn awgn_allowlisted_control_emits_one_genuine_beacon() {
    for snr in [6, 10] {
        let got = decode(snr, false, true);
        check_calls(&got, snr, false, true);
        assert_eq!(
            got.iter().filter(|(_, ty, _)| ty == "beacon").count(),
            1,
            "SNR={snr}, listed=true: {got:?}"
        );
    }
}
