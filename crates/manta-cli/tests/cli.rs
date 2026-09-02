use std::process::Command;

fn manta() -> Command {
    Command::new(env!("CARGO_BIN_EXE_manta"))
}

/// SPEC §2.1's ~2.05 s mandatory warmup(750 hops)+confirm(19 hops) floor
/// deterministically loses this 15 s scene's leading "CQ " before the real
/// detector ever promotes a track -- not a bug, same structural cause as
/// `golden_v1.rs`/`pipeline.rs`'s V1-based tests (see those files' doc
/// comments). A 15 s scene loses the ~2.05 s absolute prefix as a much
/// larger fraction than V1's full 120 s gate. Measured empirically (Task 11
/// Step 0): CER = 0.1304, deterministic (V1's fixed `noise_seed`). 0.17
/// gives headroom above that floor. See
/// docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md.
#[test]
fn gen_then_decode_prints_text() {
    let dir = tempfile::tempdir().unwrap();
    // Generate a short fixture through the library (fast), decode via the CLI.
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .arg("decode")
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let cer_val = manta_testkit::cer::cer(&manifest.keyed_texts[0], text.trim());
    assert!(
        cer_val < 0.17,
        "expected CER < 0.17 (measured floor 0.1304), got {cer_val:.4}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        text.trim()
    );
}

#[test]
fn gen_subcommand_writes_fixture_set() {
    let dir = tempfile::tempdir().unwrap();
    // NOTE: full 120 s V1 — this is also the fixture-generation smoke test.
    let out = manta()
        .args(["gen", "v1", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("v1.wav").exists());
    assert!(dir.path().join("v1.json").exists());
    assert!(dir.path().join("v1.manifest.json").exists());
}

#[test]
fn unknown_vector_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["gen", "v99", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn kiwi_host_without_freq_is_a_clean_error() {
    let out = manta()
        .args(["listen", "--kiwi-host", "example.com"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --kiwi-freq"
    );
}

#[test]
fn json_output_is_valid_and_deterministic_across_three_runs() {
    // SPEC §6 CI rule: same binary + same file, 3 runs -> identical output.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectorspec_short();
    let _ = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let runs: Vec<Vec<u8>> = (0..3)
        .map(|_| {
            let out = manta()
                .args(["decode", "--json"])
                .arg(dir.path().join("v1.wav"))
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        })
        .collect();
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
    let v: serde_json::Value = serde_json::from_slice(&runs[0]).unwrap();
    assert!(v["text"].is_string());
    assert!(v["freq_hz"].is_f64());
    assert!(v["events"].is_array());
}

/// MAN-29 review round 3: `manta decode` (the primary offline-IQ path) had
/// no `--freq-correction-ppm`, unlike `listen`/`soak` -- a user decoding a
/// recording from a source with a known oscillator correction couldn't use
/// the feature through the CLI at all.
#[test]
fn decode_freq_correction_ppm_shifts_the_reported_freq_hz() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let uncalibrated_out = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(uncalibrated_out.status.success());
    let uncalibrated: serde_json::Value = serde_json::from_slice(&uncalibrated_out.stdout).unwrap();
    let uncalibrated_freq = uncalibrated["freq_hz"].as_f64().unwrap();

    let calibrated_out = manta()
        .args(["decode", "--json", "--freq-correction-ppm", "10"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        calibrated_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&calibrated_out.stderr)
    );
    let calibrated: serde_json::Value = serde_json::from_slice(&calibrated_out.stdout).unwrap();
    let calibrated_freq = calibrated["freq_hz"].as_f64().unwrap();

    let expected = uncalibrated_freq * (1.0 + 10.0 * 1e-6);
    assert!(
        (calibrated_freq - expected).abs() < 1e-3,
        "--freq-correction-ppm 10 should scale freq_hz {uncalibrated_freq} to {expected}, got {calibrated_freq}"
    );
}

/// MAN-29 review round 5: a downward correction (negative ppm) is a normal
/// case the public validation contract explicitly supports
/// (`[-1000, 1000]`), but clap treats a leading-hyphen value as another
/// argument unless `allow_negative_numbers` is set -- so `decode`,
/// `listen`, and `soak` all rejected `--freq-correction-ppm -10` before it
/// ever reached the validator. `decode` is the only one testable without a
/// live device/file, so it stands in for all three.
#[test]
fn decode_accepts_a_negative_freq_correction_ppm() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let out = manta()
        .args(["decode", "--json", "--freq-correction-ppm", "-10"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--freq-correction-ppm -10 should be accepted, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn decode_json_includes_spots_field() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report.get("spots").is_some_and(|s| s.is_array()),
        "expected a 'spots' array field in decode --json output, got: {report}"
    );
}

/// MAN-28 Watch List: an operator running `manta decode` on a real
/// recording must be able to force-spot a callsign that fails automatic
/// validation, via `--allowlist`. `decode` is the only subcommand
/// testable without a live device/file, same rationale as the
/// freq-correction-ppm CLI tests above.
#[test]
fn decode_allowlist_spots_a_call_that_fails_cty_validation() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let without_allowlist = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(without_allowlist.status.success());
    let report: serde_json::Value = serde_json::from_slice(&without_allowlist.stdout).unwrap();
    assert!(
        !report["spots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["callsign"] == "QQ9ZZZ"),
        "QQ9ZZZ (unallocated cty prefix) must not spot without --allowlist, got: {report}"
    );

    let with_allowlist = manta()
        .args(["decode", "--json", "--allowlist", "QQ9ZZZ"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        with_allowlist.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with_allowlist.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&with_allowlist.stdout).unwrap();
    assert!(
        report["spots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["callsign"] == "QQ9ZZZ"),
        "--allowlist QQ9ZZZ should force a spot for QQ9ZZZ, got: {report}"
    );
}

/// MAN-31: an operator must be able to supply the suppression lists from
/// the CLI, not just via the library API -- this is the end-to-end proof
/// the wiring reaches production, not just `PipelineConfig` in isolation.
#[test]
fn decode_blocklist_flag_suppresses_a_callsign() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let blocklist_path = dir.path().join("bad-calls.txt");
    std::fs::write(&blocklist_path, "W1AW\n").unwrap();

    let out = manta()
        .args(["decode", "--json", "--blocklist"])
        .arg(&blocklist_path)
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["spots"].as_array().unwrap().len(),
        0,
        "blocklisted callsign must never be spotted, got: {report}"
    );
}

/// A Windows-authored suppression file commonly starts with a UTF-8 BOM
/// (`\u{feff}`); it must not defeat the first entry's match.
#[test]
fn decode_blocklist_flag_tolerates_a_leading_bom() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let blocklist_path = dir.path().join("bad-calls.txt");
    std::fs::write(&blocklist_path, "\u{feff}W1AW\n").unwrap();

    let out = manta()
        .args(["decode", "--json", "--blocklist"])
        .arg(&blocklist_path)
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["spots"].as_array().unwrap().len(),
        0,
        "a BOM-prefixed blocklist's first entry must still match, got: {report}"
    );
}

#[test]
#[cfg(feature = "soapy")]
fn soapy_driver_without_freq_and_rate_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--soapy-driver", "driver=rtlsdr"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --soapy-freq/--soapy-rate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("soapy-freq")
            || stderr.contains("soapy-rate")
            || stderr.contains("required"),
        "expected an explanatory error, got: {stderr}"
    );
}
