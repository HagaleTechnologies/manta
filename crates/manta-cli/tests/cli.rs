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
    // Validated before any file I/O (open_source/start_spot_server), so
    // nonexistent paths are fine for provoking this specific error.
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --dial-freq-hz"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
}

/// MAN-121 Scenario 1: the README's own `manta gen v1 --out /tmp/v1` output
/// is 2-channel IQ WAV at 96 kHz, which `listen --source` used to hard-reject
/// with "AudioIqSource requires 48000 Hz, got 96000" -- it should decode
/// instead, exactly as `manta decode` already does.
#[test]
fn listen_source_accepts_the_iq_wav_that_gen_writes() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["listen", "--source"])
        .arg(dir.path().join("v1.wav"))
        .arg("--json")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("requires 48000 Hz"), "stderr: {stderr}");
    assert!(out.status.success(), "stderr: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.contains("\"spot\"")),
        "expected at least one spot line, got stdout: {stdout}"
    );
}

/// MAN-121: a 2-channel IQ WAV with a sidecar already knows its own RF
/// center frequency (`WavIqSource` reads it), so `--server-config` must not
/// demand `--dial-freq-hz` for it the way it does for a plain audio source.
#[test]
fn listen_server_config_needs_no_dial_freq_for_a_sidecar_backed_iq_wav() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let toml_path = dir.path().join("server.toml");
    std::fs::write(
        &toml_path,
        "[server]\nstation_callsign = \"W5AU\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--source"])
        .arg(dir.path().join("v1.wav"))
        .arg("--server-config")
        .arg(&toml_path)
        .arg("--json")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    assert!(out.status.success(), "stderr: {stderr}");
}

/// A plain mono 48 kHz audio WAV still has no real RF reference, so the
/// existing gate must still fire for it -- only sidecar-backed IQ WAVs are
/// exempted.
#[test]
fn listen_server_config_still_requires_dial_freq_for_a_mono_wav() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rig.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for _ in 0..480 {
        w.write_sample(0.0f32).unwrap();
    }
    w.finalize().unwrap();

    let toml_path = dir.path().join("server.toml");
    std::fs::write(
        &toml_path,
        "[server]\nstation_callsign = \"W5AU\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
    )
    .unwrap();

    let out = manta()
        .args(["listen", "--source"])
        .arg(&wav)
        .arg("--server-config")
        .arg(&toml_path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
}

/// MAN-121 Decision 5: `--realtime` only decides WHEN samples are
/// delivered, never WHICH -- output must be byte-identical either way, the
/// determinism guarantee the broad review demands of any pacing change.
#[test]
fn realtime_replay_is_byte_identical_to_unpaced_replay() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join("v1.wav");

    let unpaced = manta()
        .args(["listen", "--source"])
        .arg(&wav)
        .arg("--json")
        .output()
        .unwrap();
    assert!(unpaced.status.success());

    let paced = manta()
        .args(["listen", "--source"])
        .arg(&wav)
        .arg("--json")
        .arg("--realtime")
        .output()
        .unwrap();
    assert!(
        paced.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&paced.stderr)
    );

    assert_eq!(unpaced.stdout, paced.stdout);
}

#[test]
fn realtime_requires_source() {
    let out = manta().args(["listen", "--realtime"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--source <SOURCE>"), "stderr: {stderr}");
}

/// MAN-121 Decision 6: `--loop` restarts the file at EOF, so a demo can be
/// left running past a single pass's worth of events.
#[test]
fn loop_replay_keeps_producing_events_past_the_end_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 5.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join("v1.wav");

    // One unpaced pass, for a baseline event count.
    let single_pass = manta()
        .args(["listen", "--source"])
        .arg(&wav)
        .arg("--json")
        .output()
        .unwrap();
    assert!(single_pass.status.success());
    let single_pass_lines = String::from_utf8_lossy(&single_pass.stdout).lines().count();

    // Looped: kill it after a wall-clock budget comfortably exceeding one
    // unpaced pass, so it must have wrapped at least once.
    let mut child = manta()
        .args(["listen", "--source"])
        .arg(&wav)
        .arg("--json")
        .arg("--loop")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let looped_lines = String::from_utf8_lossy(&out.stdout).lines().count();

    assert!(
        looped_lines > single_pass_lines,
        "expected --loop to produce more events than a single pass \
         ({single_pass_lines}), got {looped_lines}"
    );
}

#[test]
fn loop_requires_source() {
    let out = manta().args(["listen", "--loop"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--source <SOURCE>"), "stderr: {stderr}");
}

/// Round-14 review: an unpaced `--loop` never ends and advances its sample
/// clock ~30-40x faster than wall time, so a networked one would publish
/// spots timestamped ever further into the future to real clients. Like the
/// --dial-freq-hz gate, it is a flag error checked ahead of all file I/O,
/// so nonexistent paths still provoke exactly this message.
#[test]
fn loop_with_server_config_but_no_realtime_is_a_clean_error() {
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
            "--dial-freq-hz",
            "14027000",
            "--loop",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure for a networked unpaced loop"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--realtime"), "stderr: {stderr}");
}

/// The same combination WITH --realtime passes the flag gate -- it must
/// fail on the missing file instead, proving the gate above is about
/// pacing and not about `--loop` plus `--server-config` as such.
#[test]
fn loop_with_server_config_and_realtime_passes_the_flag_gate() {
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
            "--dial-freq-hz",
            "14027000",
            "--loop",
            "--realtime",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("also requires --realtime"),
        "the pacing gate must not fire when --realtime is given: {stderr}"
    );
    assert!(
        stderr.contains("/nonexistent.wav"),
        "expected the missing-file error instead: {stderr}"
    );
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
