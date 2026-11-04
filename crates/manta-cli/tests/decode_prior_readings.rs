//! MAN-317 acceptance checks. The selective notch is a controlled envelope
//! fixture, not a claim about the Watterson fading acceptance gap.
use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use std::process::Command;
use std::sync::Arc;

fn ambiguous_envelope(third_mark_ms: f64) -> Vec<f32> {
    ambiguous_envelope_at(third_mark_ms, 375.0)
}

fn ambiguous_envelope_at(third_mark_ms: f64, fs: f64) -> Vec<f32> {
    let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
    let (mut env, _) = manta_testkit::keyer::key_text("DE DE W1AW K", &spec, fs).unwrap();
    // Find every W's third mark from the exact keying schedule and suppress
    // its late tail. All character and word boundaries remain fixed.
    let mut elapsed_ms = 0.0;
    for (wi, word) in "DE DE W1AW K".split_whitespace().enumerate() {
        if wi != 0 {
            elapsed_ms += 420.0;
        }
        for (ci, c) in word.chars().enumerate() {
            if ci != 0 {
                elapsed_ms += 180.0;
            }
            let pattern = manta_decode::tree::pattern_for(c).unwrap();
            for (ei, mark) in pattern.chars().enumerate() {
                if ei != 0 {
                    elapsed_ms += 60.0;
                }
                let duration = if mark == '.' { 60.0 } else { 180.0 };
                if c == 'W' && ci == 0 && ei == 2 {
                    let start = ((elapsed_ms + third_mark_ms) * fs / 1000.0) as usize;
                    let end = ((elapsed_ms + duration) * fs / 1000.0) as usize;
                    env[start..end].fill(0.0);
                }
                elapsed_ms += duration;
            }
        }
    }
    env.extend(std::iter::repeat_n(0.0, fs as usize));
    env
}

fn decode_envelope(env: &[f32], enabled: bool) -> Vec<DecoderEvent> {
    let cfg = DecodeConfig {
        prior_enabled: enabled,
        prior: enabled.then(|| {
            Arc::new(manta_spot::prior::LanguagePrior::new(
                Arc::new(manta_spot::reference::ReferenceLists::new(
                    Arc::new(manta_spot::cty::Table::bundled()),
                    Arc::new(manta_spot::scp::Set::bundled()),
                )),
                manta_spot::prior::CallsHeard::parse("W1AW 1000"),
                2.0,
            )) as Arc<dyn manta_decode::prior::WordPrior>
        }),
        ..Default::default()
    };
    let mut decoder = TrackDecoder::new(17, cfg);
    let mut events = Vec::new();
    for (i, &a) in env.iter().enumerate() {
        events.extend(decoder.push_envelope(a + 0.01, i as u64 * 256));
    }
    events.extend(decoder.finish());
    events
}

#[test]
fn ambiguous_w1aw_resolves_to_listed_call_only_with_prior() {
    let env = ambiguous_envelope(102.0);
    let off = events_to_text(&decode_envelope(&env, false));
    let on = events_to_text(&decode_envelope(&env, true));
    assert!(off.contains("R1AW"), "prior-off counterfactual: {off}");
    assert!(on.contains("W1AW"), "prior-on reading: {on}");
}

#[test]
fn diagnostics_show_held_readings_and_margins() {
    let env = ambiguous_envelope(102.0);
    let first = decode_envelope(&env, true);
    assert_eq!(
        first,
        decode_envelope(&env, true),
        "deterministic event stream"
    );
    let held: Vec<_> = first
        .iter()
        .filter_map(|event| match event {
            DecoderEvent::ReadingsHeld {
                readings, settled, ..
            } => Some((readings, settled)),
            _ => None,
        })
        .collect();
    assert!(
        held.iter()
            .any(|(readings, settled)| !**settled && readings.iter().any(|(r, _)| r == "R1")),
        "{held:?}"
    );
    let settled = held
        .iter()
        .find(|(readings, settled)| **settled && readings[0].0 == "W1AW")
        .expect("settled W1AW diagnostics");
    assert_eq!(settled.0[0].1, 0.0);
    assert!(settled
        .0
        .iter()
        .any(|(r, gap)| r == "R1AW" && *gap >= 0.0 && gap.is_finite()));
    assert!(!decode_envelope(&env, false)
        .iter()
        .any(|e| matches!(e, DecoderEvent::ReadingsHeld { .. })));
}

#[test]
fn clean_unknown_call_spots_identically_with_and_without_prior() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 60.0;
    spec.signals[0].text = "CQ CQ DE K1ZQX K1ZQX K".into();
    assert!(!manta_spot::scp::Set::bundled().contains("K1ZQX"));
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let mut outputs = Vec::new();
    for prior in ["off", "on"] {
        let config = dir.path().join(format!("{prior}.toml"));
        std::fs::write(&config, format!("[decode]\nprior = \"{prior}\"\n")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["decode", "--json", "--config"])
            .arg(config)
            .arg(dir.path().join("v1.wav"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            report["spots"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["dxCall"] == "K1ZQX"),
            "prior {prior}, text {}, spots {}",
            report["text"],
            report["spots"]
        );
        outputs.push(report["spots"].clone());
    }
    assert_eq!(outputs[0], outputs[1]);
}

#[test]
fn cli_ambiguous_word_uses_calls_heard_and_reports_readings() {
    let dir = tempfile::tempdir().unwrap();
    let fs = 96_000.0;
    let env = ambiguous_envelope_at(98.0, fs);
    let amplitude = manta_testkit::noise::amplitude_for_snr_2500(20.0, fs);
    let mut iq: Vec<_> = env
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            let phase = std::f64::consts::TAU * 12_340.0 * i as f64 / fs;
            num_complex::Complex32::new(phase.cos() as f32, phase.sin() as f32) * a * amplitude
        })
        .collect();
    manta_testkit::noise::add_unit_awgn(&mut iq, 317);
    for sample in &mut iq {
        *sample *= 0.05;
    }
    let wav =
        manta_testkit::wav::write_fixture(dir.path(), "ambiguous", &iq, fs, 14_000_000.0).unwrap();
    std::fs::write(dir.path().join("calls-heard.txt"), "W1AW 1000\n").unwrap();
    let mut reports = Vec::new();
    for prior in ["off", "on"] {
        let config = dir.path().join(format!("{prior}.toml"));
        std::fs::write(
            &config,
            format!(
                "[decode]\nprior = \"{prior}\"\n[spot]\ncalls_heard_path = \"calls-heard.txt\"\n"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["decode", "--json", "--config"])
            .arg(config)
            .arg(&wav)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        reports.push(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap());
    }
    assert!(
        reports[0]["text"].as_str().unwrap().contains("R1AW"),
        "off {}",
        reports[0]["text"]
    );
    assert!(
        reports[1]["text"].as_str().unwrap().contains("W1AW"),
        "on {}",
        reports[1]["text"]
    );
    assert!(reports[1]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"] == "ReadingsHeld" && e["settled"] == true));
}
