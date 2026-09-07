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
fn server_config_without_dial_freq_for_audio_source_is_a_clean_error() {
    // MAN-74: the server now starts iff the resolved config has a
    // [server] table (Decision 6), so this check needs an actual,
    // parseable file with [server] present -- a nonexistent path (the
    // pre-MAN-74 fixture) fails at config::load's file-read instead of
    // ever reaching the dial-freq-hz check.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(&cfg_path, "[server]\nstation_callsign = \"W3XYZ\"\n").unwrap();
    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --dial-freq-hz"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
}

#[test]
fn dial_freq_hz_rejects_non_finite_and_non_positive_values() {
    for bad in ["nan", "inf", "-inf", "0", "-14027000"] {
        let out = manta()
            .args([
                "listen",
                "--source",
                "/nonexistent.wav",
                "--dial-freq-hz",
                bad,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--dial-freq-hz {bad} should have been rejected"
        );
    }
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

/// MAN-74 "What We're NOT Doing" #2: `decode` is the entry point SPEC §6's
/// "file input -> byte-identical spot logs" determinism contract runs
/// through, so it must stay hermetic against both `MANTA_*` env vars and
/// `--config` -- an ambient, machine-specific override would make the
/// contract depend on the environment instead of the input file alone.
#[test]
fn manta_decode_ignores_every_manta_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectorspec_short();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let without_env = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        without_env.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&without_env.stderr)
    );

    let with_env = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .env("MANTA_DECODE_TIMING_SIGMA", "0.9")
        .env("MANTA_DETECTOR_ON_SNR_DB", "99.0")
        .output()
        .unwrap();
    assert!(
        with_env.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with_env.stderr)
    );
    assert_eq!(
        without_env.stdout, with_env.stdout,
        "manta decode must be hermetic against MANTA_* environment variables"
    );

    let with_config_flag = manta()
        .args(["decode", "--config", "/x.toml"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        !with_config_flag.status.success(),
        "`decode` must have no --config flag"
    );
}

/// Code-review finding 4: `config::load` used to call `std::env::vars()`,
/// which panics if ANY variable in the whole process environment (not just
/// a `MANTA_*` one) has a non-UTF-8 name or value -- turning an unrelated
/// stray variable into a crash on every `listen`/`soak` startup. Uses a
/// `MANTA_*`-prefixed name (the case most likely to actually reach the
/// overlay logic) with a non-UTF-8 VALUE to prove the fix, not just that
/// an unrelated variable is ignored.
#[test]
#[cfg(unix)]
fn a_non_utf8_environment_variable_does_not_panic_the_config_loader() {
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);

    let bad_value = std::ffi::OsStr::from_bytes(&[0xff, 0xfe, 0xfd]);
    let out = manta()
        .args(["listen", "--source"])
        .arg(&wav_path)
        .env("MANTA_DETECTOR_ON_SNR_DB", bad_value)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a non-UTF-8 environment variable must not crash config::load -- stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// Writes a real (non-IQ) mono 48 kHz WAV of a keyed CW tone -- the format
/// `AudioIqSource::from_wav_file`/`--source`/`[input] type = "file"`
/// actually consume, distinct from `manta_testkit::vectors::write_fixture_set`'s
/// stereo IQ WAV (`decode`'s format). Mirrors
/// `tests/soak_ci.rs`'s in-process technique, but written to disk since
/// these tests drive the real `manta` binary as a subprocess.
fn write_real_audio_wav(path: &std::path::Path, text: &str, duration_s: f64) {
    let fs = manta_input::TARGET_RATE_HZ;
    let spec = manta_testkit::keyer::KeyerSpec::new(25.0);
    let (env, _keyed) =
        manta_testkit::keyer::key_text_loop(text, &spec, fs as f64, duration_s).unwrap();
    let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
    let mut phi = 0.0f64;
    let samples: Vec<f32> = env
        .iter()
        .map(|&e| {
            let s = e * phi.cos() as f32;
            phi += dphi;
            s
        })
        .collect();
    let wav_spec = hound::WavSpec {
        channels: 1,
        sample_rate: fs,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, wav_spec).unwrap();
    for s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
}

/// Writes `duration_s` of pure silence in the same format
/// `write_real_audio_wav` does -- long enough to clear `listen()`'s
/// `CALIBRATION_SECONDS` startup read, guaranteed to never produce a
/// `CharDecoded`/spot event. Used by the Decision 5 override test below to
/// distinguish "the config's source ran" from "the CLI's source ran"
/// without depending on decode text quality across the whole multi-channel
/// passband, which a raw text-mode stream mixes across every track.
fn write_silent_wav(path: &std::path::Path, duration_s: f64) {
    let fs = manta_input::TARGET_RATE_HZ;
    let n = (fs as f64 * duration_s) as usize;
    let wav_spec = hound::WavSpec {
        channels: 1,
        sample_rate: fs,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, wav_spec).unwrap();
    for _ in 0..n {
        w.write_sample(0.0f32).unwrap();
    }
    w.finalize().unwrap();
}

/// Runs `manta listen --json --source <wav>`, optionally with `config_toml`
/// written to a fresh `--config` file in `dir` -- shared by the
/// `[detector]`-wiring regression test below.
fn run_listen(wav: &std::path::Path, dir: &std::path::Path, config_toml: Option<&str>) -> Vec<u8> {
    let mut cmd = manta();
    cmd.args(["listen", "--json", "--source"]).arg(wav);
    if let Some(toml) = config_toml {
        let cfg_path = dir.join("detector.toml");
        std::fs::write(&cfg_path, toml).unwrap();
        cmd.args(["--config"]).arg(&cfg_path);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// MAN-74 Phase 2 regression: on the pre-fix commit, `[detector]` parsed
/// (Phase 1) but `resolve_pipeline` never reached the running detector, so
/// a config with a high `on_snr_db` decoded byte-identically to no config
/// at all. This proves the wiring, not just the parse.
///
/// `on_snr_db = 1000.0`, not something closer to the 12.0 default:
/// `write_real_audio_wav` synthesizes a pure keyed tone with no added
/// noise, so its real per-channel SNR (set only by float32 rounding/FFT
/// leakage, not a calibrated noise floor like the golden vectors'
/// `manta_testkit::vectors` AWGN) is far higher than any realistic HF
/// signal's -- a threshold has to clear that to suppress every track
/// reliably rather than flake on the exact leakage floor this build
/// happens to produce.
#[test]
fn detector_on_snr_db_from_the_config_file_actually_silences_the_decode() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);

    let baseline = run_listen(&wav_path, dir.path(), None);
    let muted = run_listen(
        &wav_path,
        dir.path(),
        Some("[detector]\non_snr_db = 1000.0\n"),
    );

    assert!(!baseline.is_empty(), "baseline must decode something");
    assert!(
        muted.is_empty(),
        "detector.on_snr_db = 1000.0 must suppress every track, got: {}",
        String::from_utf8_lossy(&muted)
    );
}

/// MAN-74 scenario 1, end to end: `manta listen --config manta.toml` with
/// an `[input] type = "file"` table and NO CLI source flags at all must
/// decode using exactly that source configuration.
#[test]
fn listen_runs_from_a_config_file_with_no_source_flags() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(&cfg_path, "[input]\ntype = \"file\"\npath = \"cw.wav\"\n").unwrap();

    let out = manta()
        .args(["listen", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "must have decoded something from the config-only [input] source"
    );
}

/// MAN-74 scenario 2, end to end: a config file with a table `manta`
/// doesn't model exits non-zero, naming the table.
#[test]
fn unknown_top_level_config_table_is_rejected_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[server]\nstation_callsign = \"W3XYZ\"\n\n[completely_bogus_table]\nnonsense = 42\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unmodeled table must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("completely_bogus_table"),
        "stderr must name the table: {stderr}"
    );
}

/// MAN-74 round-2 finding C-1: `MANTA_CONFIG` is documented (SPEC §9,
/// `docs/DECISIONS/2026-09-06-man74-config-surface.md`) as the `--config`
/// fallback an operator's systemd unit can set instead of a CLI flag, but
/// `main.rs` never read it. Reuses scenario 2's unknown-table repro so a
/// pass here proves the file was actually loaded via the env var, not just
/// that the process didn't crash.
#[test]
fn manta_config_env_var_is_used_as_the_config_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[server]\nstation_callsign = \"W3XYZ\"\n\n[completely_bogus_table]\nnonsense = 42\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav"])
        .env("MANTA_CONFIG", &cfg_path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "MANTA_CONFIG must be read as the --config fallback and reject the unknown table"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("completely_bogus_table"),
        "stderr must show the config file reached via MANTA_CONFIG was actually parsed: {stderr}"
    );
}

/// MAN-74 round-2 finding C-1 (continued): an explicit `--config` flag
/// still wins over `MANTA_CONFIG`, matching the documented CLI > env
/// precedence -- `MANTA_CONFIG` pointing at the bogus-table file above must
/// not override a valid `--config` file.
#[test]
fn explicit_config_flag_beats_the_manta_config_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let bogus_cfg_path = dir.path().join("bogus.toml");
    std::fs::write(&bogus_cfg_path, "[completely_bogus_table]\nnonsense = 42\n").unwrap();
    let good_cfg_path = dir.path().join("good.toml");
    std::fs::write(&good_cfg_path, "").unwrap();

    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav", "--config"])
        .arg(&good_cfg_path)
        .env("MANTA_CONFIG", &bogus_cfg_path)
        .output()
        .unwrap();
    // Both configs are otherwise valid enough to reach the WAV-open step,
    // so success/failure alone can't tell them apart -- but the bogus file
    // would have failed at config-load with a distinct, checkable error.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("completely_bogus_table"),
        "an explicit --config must win over MANTA_CONFIG, got: {stderr}"
    );
}

/// MAN-74 scenario 3, end to end: an explicit `--freq-correction-ppm 0`
/// must win over a nonzero file value -- the exact `default_value_t = 0.0`
/// trap this ticket's flag redesign (`Option<f64>`) exists to avoid. Uses
/// `input.dial_freq_hz = 14025000.0` so 10 ppm is a ~140 Hz shift, well
/// above decode jitter -- at audio baseband alone (~700 Hz) 10 ppm is
/// ~0.007 Hz, undetectable against real estimator noise.
fn last_track_meta_freq_hz(stdout: &[u8]) -> f64 {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("TrackMeta"))
        .filter_map(|v| v.get("freq_hz").and_then(|f| f.as_f64()))
        .next_back()
        .expect("expected at least one TrackMeta event")
}

#[test]
fn cli_freq_correction_ppm_beats_the_config_file_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[input]\ntype = \"audio\"\nfreq_correction_ppm = 10.0\ndial_freq_hz = 14025000.0\n",
    )
    .unwrap();

    let run = |extra_args: &[&str]| {
        let out = manta()
            .args(["listen", "--json", "--source"])
            .arg(&wav_path)
            .args(["--config"])
            .arg(&cfg_path)
            .args(extra_args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        last_track_meta_freq_hz(&out.stdout)
    };

    let from_file_ppm = run(&[]);
    // cli_zero_ppm is the raw, uncorrected frequency: --freq-correction-ppm 0
    // must win over the file's 10.0, not be mistaken for "flag absent".
    let cli_zero_ppm = run(&["--freq-correction-ppm", "0"]);

    let expected_from_file = cli_zero_ppm * (1.0 + 10.0 * 1e-6);
    assert!(
        (from_file_ppm - expected_from_file).abs() < 1.0,
        "file's freq_correction_ppm = 10.0 should have scaled freq_hz {cli_zero_ppm} to \
         {expected_from_file}, got {from_file_ppm} -- if the CLI's explicit 0 had been \
         mistaken for 'flag absent', both runs would report the same (scaled) frequency"
    );
    assert!(
        (from_file_ppm - cli_zero_ppm).abs() > 10.0,
        "the two runs should differ by ~140 Hz (10 ppm at 14 MHz); got from_file={from_file_ppm} cli_zero={cli_zero_ppm}"
    );
}

/// Plan-named test: `run` is `listen`'s clap `visible_alias`, so the
/// ticket's literal Gherkin spelling (`manta run --config manta.toml`)
/// must actually work, not merely compile.
#[test]
fn run_is_a_visible_alias_of_listen() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(&cfg_path, "[input]\ntype = \"file\"\npath = \"cw.wav\"\n").unwrap();

    let out = manta()
        .args(["run", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "manta run --config must decode exactly like manta listen --config"
    );
}

/// Plan-named test: `--server-config` is `--config`'s deprecated alias
/// (kept for existing systemd unit files) -- reuses scenario 2's
/// unknown-table repro, like `manta_config_env_var_is_used_as_the_config_fallback`
/// does for `MANTA_CONFIG`, so a pass proves the file was actually loaded
/// via the old spelling rather than merely that the process didn't crash.
#[test]
fn server_config_still_works_as_a_deprecated_alias_of_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[server]\nstation_callsign = \"W3XYZ\"\n\n[completely_bogus_table]\nnonsense = 42\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav", "--server-config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--server-config must still be read as the --config alias"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("completely_bogus_table"),
        "stderr must show the config file reached via --server-config was actually parsed: {stderr}"
    );
}

/// Plan-named test: `soak --config` resolves `[input]` the same way the
/// config-driven `listen` path does (Phase 5) -- `soak` never starts the
/// telnet/JSON/metrics servers, so this exercises source resolution only.
#[test]
fn soak_accepts_config_too() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 5.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(&cfg_path, "[input]\ntype = \"file\"\npath = \"cw.wav\"\n").unwrap();

    let out = manta()
        .args(["soak", "--duration", "5", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SoakReport"),
        "expected a SoakReport on stderr, got: {stderr}"
    );
}

/// Plan-named test (MAN-74 Decision 5): ANY CLI source-selection flag
/// discards `[input]` WHOLESALE, not merely the specific key it
/// corresponds to -- proven by making the config's file source pure
/// silence (guaranteed to decode to nothing) and the CLI's `--source` file
/// a real keyed signal, so which one actually ran is directly observable
/// as empty-vs-non-empty output, with no dependence on decode text quality
/// across the whole multi-channel passband.
#[test]
fn any_source_flag_overrides_the_whole_input_source() {
    let dir = tempfile::tempdir().unwrap();
    let configured_wav = dir.path().join("configured.wav");
    write_silent_wav(&configured_wav, 20.0);
    let cli_wav = dir.path().join("cli.wav");
    write_real_audio_wav(&cli_wav, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[input]\ntype = \"file\"\npath = \"configured.wav\"\n",
    )
    .unwrap();

    // Sanity check: the config's own source, used alone (no CLI source
    // flags), really is inert -- otherwise a pass below would prove
    // nothing.
    let config_only = manta()
        .args(["listen", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        config_only.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&config_only.stderr)
    );
    assert!(
        config_only.stdout.is_empty(),
        "a silent [input] source must decode to nothing, got: {}",
        String::from_utf8_lossy(&config_only.stdout)
    );

    let out = manta()
        .args(["listen", "--source"])
        .arg(&cli_wav)
        .args(["--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.stdout.is_empty(),
        "a CLI --source flag must discard [input] wholesale -- decoding the silent \
         configured.wav instead of cli.wav would produce no output at all"
    );
}

/// Plan-named test: an `[input]` table selecting a feature-gated source
/// type (`soapy`) built WITHOUT that feature fails with a message naming
/// the required `--features` flag (`open_source_spec`), not a generic or
/// opaque error.
#[test]
#[cfg(not(any(feature = "soapy", feature = "hpsdr")))]
fn a_feature_gated_source_type_fails_with_a_message_naming_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(
        &cfg_path,
        "[input]\ntype = \"soapy\"\ndriver = \"driver=rtlsdr\"\nfreq_hz = 14025000.0\nrate_hz = 192000.0\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a soapy [input] source built without --features soapy must fail cleanly"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--features soapy"),
        "stderr must name the required feature: {stderr}"
    );
}

/// Polls `addr` with short-lived TCP connect attempts until one succeeds
/// or `timeout` elapses. Used by the Decision 8 server-startup tests below
/// to observe an actually bound socket, not just an exit code.
fn wait_for_port_open(addr: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// MAN-74 Decision 8: the telnet/JSON/metrics servers start iff the
/// resolved config has a `[server]` table -- not merely because `--config`
/// was given. Proven by actually connecting a TCP client to the telnet
/// port: `start_spot_server` (`main.rs`) binds all three sockets before
/// `listen()` ever reads a sample, so the window to observe this is the
/// whole run, not a narrow race.
/// Binds an OS-assigned ephemeral port and immediately releases it --
/// CLAUDE.md's multi-agent hygiene rule ("don't bind fixed ports") means a
/// literal port number here can false-pass or false-fail against another
/// concurrent test/agent on the same host.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn config_with_a_server_table_starts_the_servers() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    let (telnet_port, json_port, metrics_port) = (free_port(), free_port(), free_port());
    std::fs::write(
        &cfg_path,
        format!(
            "[server]\nstation_callsign = \"W3XYZ\"\ntelnet_port = {telnet_port}\n\
             json_port = {json_port}\nmetrics_port = {metrics_port}\n\n\
             [input]\ntype = \"file\"\npath = \"cw.wav\"\ndial_freq_hz = 14025000.0\n"
        ),
    )
    .unwrap();

    let mut child = manta()
        .args(["listen", "--config"])
        .arg(&cfg_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let connected = wait_for_port_open(
        &format!("127.0.0.1:{telnet_port}"),
        std::time::Duration::from_secs(10),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        connected,
        "telnet_port must be bound when [server] is present"
    );
}

/// MAN-74 Decision 8, the negative case: with NO `[server]` table at all,
/// the servers must not even attempt to bind a socket -- proven by
/// pre-occupying the REAL default ports (`manta-server/src/config.rs`'s
/// `default_telnet_port`/`default_json_port`/`default_metrics_port`:
/// 7300/7301/7302) a `[server]`-less config would fall back to if the
/// Decision 8 guard ever regressed to constructing a `ServerConfig`
/// unconditionally; if the [server]-absent path tried to bind any of them
/// anyway, the whole `listen` command would fail with an "address in use"
/// error and exit non-zero instead of decoding cleanly. Deliberately NOT
/// `free_port()` here (unlike the positive test above) -- an OS-assigned
/// port the regression doesn't know to try would prove nothing.
#[test]
fn config_without_a_server_table_does_not_start_the_servers() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("cw.wav");
    write_real_audio_wav(&wav_path, "CQ CQ DE W1AW W1AW K", 20.0);
    let cfg_path = dir.path().join("manta.toml");
    std::fs::write(&cfg_path, "[input]\ntype = \"file\"\npath = \"cw.wav\"\n").unwrap();

    let _held_telnet = std::net::TcpListener::bind("127.0.0.1:7300").unwrap();
    let _held_json = std::net::TcpListener::bind("127.0.0.1:7301").unwrap();
    let _held_metrics = std::net::TcpListener::bind("127.0.0.1:7302").unwrap();

    let out = manta()
        .args(["listen", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "servers must not be attempted with no [server] table -- stderr: {}",
        String::from_utf8_lossy(&out.stderr)
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

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_host_without_freq_and_rate_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--hpsdr-host", "192.168.1.100"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --hpsdr-freq/--hpsdr-rate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hpsdr-freq")
            || stderr.contains("hpsdr-rate")
            || stderr.contains("required"),
        "expected an explanatory error, got: {stderr}"
    );
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_flags_are_recognized_by_listen_and_soak() {
    // Flag-recognition smoke test (MAN-51 acceptance): confirms
    // --hpsdr-host/--hpsdr-port/--hpsdr-freq/--hpsdr-rate exist on both
    // subcommands per the ticket's Gherkin -- clap must not reject them as
    // unknown arguments. Checked via --help rather than a real invocation
    // (round-1 review finding): actually running `listen`/`soak` with a
    // plausible LAN host like 192.168.1.100 risks an unbounded hang on any
    // machine where something really answers on that address -- `--help`
    // proves flag recognition with zero I/O. Connecting to a real device
    // is MAN-52's job.
    for sub in ["listen", "soak"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([sub, "--help"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{sub} --help should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        for flag in [
            "--hpsdr-host",
            "--hpsdr-port",
            "--hpsdr-freq",
            "--hpsdr-rate",
        ] {
            assert!(
                stdout.contains(flag),
                "{sub} --help should list {flag}, got: {stdout}"
            );
        }
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_rate_rejects_non_finite_values() {
    // Round-1 review finding: `HpsdrConfig::validate`'s bandwidth check
    // silently passes NaN (comparisons against NaN are always false), and
    // the value then reaches `GapDetector::new`'s
    // `Duration::from_secs_f64`, which panics. Caught at CLI-parse time
    // instead, before any source is opened.
    // "-inf"/negative values aren't exercised here: clap treats a leading
    // "-" as a new flag rather than this value (a separate, pre-existing
    // parsing behavior, not part of the NaN-panic finding this test
    // covers) unless `allow_negative_numbers` is set, which this flag
    // deliberately doesn't need since every legitimate rate is positive.
    // "1e-20" covers the round-2 finding: finite and positive, but still
    // small enough to overflow `Duration::from_secs_f64` downstream.
    for bad_rate in ["NaN", "inf", "0", "1e-20"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "listen",
                "--hpsdr-host",
                "192.168.1.100",
                "--hpsdr-freq",
                "14000000",
                "--hpsdr-rate",
                bad_rate,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--hpsdr-rate {bad_rate} should be rejected before any I/O"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("hpsdr-rate"),
            "expected an explanatory error for --hpsdr-rate {bad_rate}, got: {stderr}"
        );
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_freq_rejects_non_finite_and_non_positive_values() {
    // Round-2 review finding: --hpsdr-freq was never validated at all --
    // `HpsdrConfig` only length-checks `center_freq_hz`, not its values, so
    // NaN/inf/non-positive input propagated into every emitted spot's
    // frequency field. Matches `parse_dial_freq_hz`'s validation.
    for bad_freq in ["NaN", "inf", "-inf", "0", "-14000000"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "listen",
                "--hpsdr-host",
                "192.168.1.100",
                "--hpsdr-freq",
                bad_freq,
                "--hpsdr-rate",
                "192000",
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--hpsdr-freq {bad_freq} should be rejected before any I/O"
        );
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_host_conflicts_with_kiwi_host() {
    // Round-1 review finding: without this, clap accepted --hpsdr-* and
    // --kiwi-* together, opened the HPSDR source first, but reported the
    // Kiwi source name to the server's health metrics -- silently ignoring
    // the requested Kiwi source.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            "listen",
            "--hpsdr-host",
            "192.168.1.100",
            "--hpsdr-freq",
            "14000000",
            "--hpsdr-rate",
            "192000",
            "--kiwi-host",
            "example.com",
            "--kiwi-freq",
            "14000000",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--hpsdr-host and --kiwi-host together should be a clean clap error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "expected a clap conflict error, got: {stderr}"
    );
}
