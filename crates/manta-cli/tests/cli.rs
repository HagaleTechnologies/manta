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

/// `Engine::Hsmm` is a fully implemented, reviewed engine since Task 8
/// (`TrackDecoder::push_hop_hsmm`) and, as of Task 11, is no longer
/// rejected by `parse_engine` on any command: `--engine hsmm` must run the
/// real decode pipeline end to end (not just parse), the same as `legacy`/
/// `edge-legacy`.
#[test]
fn decode_engine_hsmm_runs_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["decode", "--json", "--engine", "hsmm"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "must not panic; stderr: {stderr}"
    );
    assert!(
        out.status.success(),
        "--engine hsmm must run successfully; stderr: {stderr}"
    );
    // A parseable DecodeReport proves the hsmm engine ran the full
    // decode -> JSON-report pipeline, not just that clap accepted the flag.
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["events"].is_array());
}

#[test]
fn run_is_the_canonical_daemon_verb() {
    // MAN-77 scenario 1. Repro on e398d46: `manta run --help` exited 2 with
    // "error: unrecognized subcommand 'run'".
    let out = manta().args(["run", "--help"]).output().unwrap();
    assert!(out.status.success(), "manta run --help should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: manta run"), "stdout: {stdout}");

    let top = manta().arg("--help").output().unwrap();
    let top = String::from_utf8_lossy(&top.stdout);
    // `run` is listed as a command; `listen` appears only as its alias.
    assert!(top.contains("  run "), "top-level help: {top}");
    assert!(top.contains("[alias: listen]"), "top-level help: {top}");
}

#[test]
fn listen_is_still_accepted_as_an_alias_of_run() {
    // The ticket's "existing scripts don't break silently" requirement.
    let out = manta().args(["listen", "--help"]).output().unwrap();
    assert!(
        out.status.success(),
        "manta listen --help should still succeed"
    );
}

#[test]
fn decode_and_gen_are_unaffected_by_the_verb_promotion() {
    // MAN-77 scenario 2, asserted explicitly rather than left implicit.
    for sub in ["decode", "gen"] {
        let out = manta().args([sub, "--help"]).output().unwrap();
        assert!(out.status.success(), "manta {sub} --help should succeed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("Usage: manta {sub}")),
            "{sub}: {stdout}"
        );
    }
}

#[test]
fn config_is_the_canonical_daemon_config_flag() {
    // Repro on e398d46: "error: unexpected argument '--config' found".
    // Validated before any file I/O, so nonexistent paths provoke the
    // --dial-freq-hz error, which proves --config was accepted and routed
    // to the same field --server-config used to reach.
    let out = manta()
        .args([
            "run",
            "--source",
            "/nonexistent.wav",
            "--config",
            "/nonexistent.toml",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    // The error text must name the new flag, not the old one.
    assert!(stderr.contains("--config"), "stderr: {stderr}");
    assert!(
        !stderr.contains("--server-config"),
        "stale flag name: {stderr}"
    );
}

#[test]
fn server_config_is_still_accepted_as_a_hidden_alias_of_config() {
    let out = manta()
        .args([
            "run",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--dial-freq-hz"));

    // Hidden: help advertises the canonical name only.
    let help = manta().args(["run", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--config <CONFIG>"), "help: {help}");
    assert!(
        !help.contains("--server-config"),
        "deprecated flag advertised: {help}"
    );
}

#[test]
fn deprecated_daemon_spelling_warns_on_stderr_and_names_the_replacement() {
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`manta run --config`"), "stderr: {stderr}");
    assert!(stderr.contains("`--config`"), "stderr: {stderr}");
}

#[test]
fn capture_rate_hz_that_does_not_evenly_divide_the_source_rate_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 48_000.0; // AudioIqSource requires exactly 48000 Hz native
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "20000"]) // 48000/20000 is not an integer
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--capture-rate-hz") || stderr.contains("power of two"),
        "stderr: {stderr}"
    );
}

#[test]
fn capture_rate_hz_that_divides_evenly_decimates_and_still_decodes() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 48_000.0; // AudioIqSource requires exactly 48000 Hz native
    spec.duration_s = 10.0; // short scene, this test only proves the wiring runs end-to-end
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "24000"]) // 48000 -> 24000, factor 2
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn capture_rate_hz_replays_a_2channel_iq_wav_through_wav_iq_source() {
    // MAN-169 round-2 Codex finding: `open_audio_source` used to route
    // every `--source <path>.wav` through `AudioIqSource::from_wav_file`
    // unconditionally, which hard-rejects every rate but 48000 Hz -- so a
    // 96/192 kS/s raw complex-IQ replay (the format `decode`/`oracle`
    // already read directly) could never reach `--capture-rate-hz`'s
    // decimation wrapper via the CLI at all; only golden-vector tests that
    // called `Decimator` directly (`golden_decimated_capture.rs`) ever
    // exercised that combination. This drives the real `run --source ...
    // --capture-rate-hz ...` CLI path end-to-end against a genuine
    // 2-channel 96 kHz IQ WAV to prove `open_audio_source` now detects the
    // 2-channel case and routes it through `WavIqSource` instead, unlocking
    // decimated file replay the same way it already works for live SDR
    // sources.
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 96_000.0;
    spec.duration_s = 10.0; // short scene, this test only proves the wiring runs end-to-end
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--capture-rate-hz", "48000"]) // 96000 -> 48000, factor 2
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn without_source_iq_a_2channel_wav_is_still_treated_as_stereo_audio() {
    // MAN-169 round-4 Codex finding (Finding A): channel count alone can't
    // distinguish a genuine 2-channel raw-IQ capture from an ordinary
    // stereo real-audio recording -- both `WavIqSource` and `AudioIqSource`
    // accept 2-channel WAVs. Without `--source-iq`, `--source` must always
    // go through `AudioIqSource::from_wav_file` (the pre-round-2, and
    // pre-this-PR, default), never `WavIqSource`. Proven indirectly: v1()'s
    // default fs is 96000 Hz, and `AudioIqSource::from_wav_file` hard-
    // rejects every rate but 48000 -- so this must fail with that source's
    // own "48000" error, not a `WavIqSource`-shaped success or a different
    // error, proving the 2-channel WAV was never silently reinterpreted as
    // IQ.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1(); // fs=96_000, 2-channel WAV, no --source-iq
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("48000"),
        "expected AudioIqSource's rate-mismatch error, got: {stderr}"
    );
    assert!(
        !stderr.contains("IQ WAV must have"),
        "must not go through WavIqSource without --source-iq: {stderr}"
    );
}

#[test]
fn config_does_not_require_dial_freq_hz_for_an_iq_wav_with_a_real_sidecar() {
    // MAN-169 round-3 Codex finding: `has_rf_aware_source` (the gate behind
    // `--dial-freq-hz is required with --config`) only checked kiwi/soapy/
    // hpsdr CLI flags -- a 2-channel IQ WAV replay with a real
    // `<stem>.json` sidecar center frequency (the same file format Task 2's
    // `WavIqSource` round-2 fix unlocked for --capture-rate-hz) was still
    // wrongly rejected as "not RF-aware" and forced a redundant
    // --dial-freq-hz, even though the source already reports a real RF
    // center via WavIqSource::center_freq_hz(). This proves the gate no
    // longer fires for that case -- the run still fails (the --config path
    // doesn't exist), but it must fail for THAT reason, not the
    // --dial-freq-hz one, proving the RF-awareness check itself now passes.
    // Requires --source-iq (round-4: no more channel-count sniffing).
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1(); // fs=96_000, center_freq_hz=14_000_000 (nonzero)
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--config", "/nonexistent-daemon-config.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("--dial-freq-hz"),
        "RF-awareness gate should not fire for an IQ WAV with a real sidecar: {stderr}"
    );
}

#[test]
fn config_requires_dial_freq_hz_for_an_iq_wav_with_a_zero_sidecar_center() {
    // MAN-169 round-4 Codex finding (Finding B): a `<stem>.json` sidecar
    // existing is not proof its `center_freq_hz` is meaningful --
    // `center_freq_hz: 0.0` is `WavIqSource`'s own "unknown center"
    // sentinel (the same value it reports when there's no sidecar at all),
    // so existence-only checking wrongly bypassed the --dial-freq-hz guard
    // for a source that doesn't actually report a real RF center. This
    // proves the opposite of the sibling "real sidecar" test above: the
    // guard must still fire when the sidecar's value is the zero sentinel.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        center_freq_hz: 0.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--config", "/nonexistent-daemon-config.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--dial-freq-hz"),
        "a sidecar with center_freq_hz: 0.0 must not bypass the --dial-freq-hz guard: {stderr}"
    );
}

#[test]
fn capture_rate_hz_rejects_non_finite_and_degenerately_small_values() {
    // MAN-169 whole-branch review finding: a small --capture-rate-hz (e.g.
    // 187.5 Hz, reachable as 48000/256) resolves to a Channelizer with
    // hop=0, which hangs Channelizer::process's read-advancing loop
    // forever. Caught here, at CLI-parse time -- before any source is
    // opened -- via parse_capture_rate_hz's MIN_CAPTURE_RATE_HZ floor, not
    // just later at Decimator::new's own construction-time check.
    // "-inf"/negative values aren't exercised here, same reasoning as
    // hpsdr_rate_rejects_non_finite_values above: clap treats a leading
    // "-" as a new flag rather than this value unless
    // `allow_negative_numbers` is set, which this flag doesn't need since
    // every legitimate rate is positive.
    for bad_rate in ["NaN", "inf", "0", "187.5", "500"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "run",
                "--source",
                "/nonexistent-for-this-test.wav",
                "--capture-rate-hz",
                bad_rate,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--capture-rate-hz {bad_rate} should be rejected before any I/O"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("capture-rate-hz"),
            "expected an explanatory error for --capture-rate-hz {bad_rate}, got: {stderr}"
        );
        assert!(
            !stderr.contains("nonexistent-for-this-test"),
            "should fail at CLI-parse time, before the source file is ever opened: {stderr}"
        );
    }
}

#[test]
fn the_ad_hoc_listen_path_is_not_nagged() {
    // The ticket title keeps `listen` for audio/dev testing, and
    // docs/RUNBOOKS/m1-w1aw-live-copy.md still instructs `listen --device`.
    let out = manta()
        .args(["listen", "--kiwi-host", "example.com"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("deprecated"), "unexpected nag: {stderr}");
}

#[test]
fn deprecation_notices_never_touch_stdout() {
    // AGENTS.md: file input -> byte-identical spot logs. stdout carries the
    // JSON Lines stream; a warning there would corrupt it. Uses the same
    // argv as `deprecated_daemon_spelling_warns_on_stderr_and_names_the_replacement`
    // (which does emit both notices) -- `listen --help` emits no notice at
    // all, so it can't catch an eprintln!->println! regression.
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("deprecated"), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated"),
        "test is vacuous unless a notice actually fires; stderr: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn non_utf8_argv_does_not_panic() {
    // Regression: warn_deprecations() used to scan std::env::args(), which
    // panics on non-UTF-8 argv. It runs as main()'s first statement, before
    // Cli::parse() (which uses args_os() via clap and tolerates non-UTF-8
    // paths) ever sees the argv -- so this must not panic for ANY
    // subcommand, not only the deprecated spellings. Filenames are byte
    // strings on Linux/macOS and need not be UTF-8.
    use std::os::unix::ffi::OsStrExt as _;
    let bad_path = std::ffi::OsStr::from_bytes(b"/tmp/man77-non-utf8-\xff.wav");
    let out = manta().arg("decode").arg(bad_path).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    // The nonexistent (and non-UTF-8-named) file should fail like any other
    // missing file, not crash the argv scan before Cli::parse() runs.
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

/// SPEC v2 §0/§7: `manta listen` gets the same `--engine` flag `manta
/// decode` already has (Task 6), threaded through to the same
/// `PipelineConfig`/`DecodeConfig` `manta_engine::listen` reads (Task 9).
/// `hsmm` (Task 8, no longer CLI-gated as of Task 11) is included alongside
/// `legacy`/`edge-legacy` -- all three are recognized `Engine` values with
/// no rejection anywhere in this command.
/// A full decode-success run (as `decode_accepts_engine_flag`, Task 9
/// brief, does for `decode`) isn't used here: `--source` requires a real
/// 48 kHz mono audio WAV (`AudioIqSource`, not `decode`'s 96 kHz complex-IQ
/// vector format), and a synthetic clean one hits a pre-existing,
/// `#[ignore]`'d `AudioIqSource`/Hilbert near-DC leakage bug
/// (`manta-engine`'s `listen_decodes_a_clean_real_audio_signal`,
/// <https://github.com/HagaleTechnologies/manta/issues/21>) that spuriously
/// promotes extra tracks -- not something Task 9 should newly depend on
/// being fixed. Instead: for each valid engine value, confirm clap accepts
/// the flag (exit code is NOT clap's arg-error 2) and the run fails for the
/// EXPECTED downstream reason (the nonexistent source file), proving
/// `--engine` parsed successfully and `merge_cli_engine`/
/// `load_decode_config_file` ran without erroring before ever reaching
/// `open_source`.
#[test]
fn listen_accepts_engine_flag_for_every_valid_value() {
    for engine in ["legacy", "edge-legacy", "hsmm"] {
        let out = manta()
            .args(["listen", "--engine", engine, "--source", "/nonexistent.wav"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{engine}: expected a failure");
        assert_ne!(
            out.status.code(),
            Some(2),
            "{engine}: --engine must not be rejected as a bad argument"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("nonexistent.wav") || stderr.contains("No such file"),
            "{engine}: expected the nonexistent-source-file error, got: {stderr}"
        );
    }
}

/// Regression, black-box: SPEC v2 §7 requires an explicit `--engine` to
/// override `[decode]`'s `engine` key. An earlier version validated
/// `engine = "hsmm"` at TOML-deserialize time -- BEFORE the CLI override
/// was ever consulted -- so a config file staging `engine = "hsmm"` failed
/// immediately even with `--engine legacy` on the command line, and the
/// override never got a chance to run. As of Task 11 `hsmm` is no longer
/// CLI-gated at all, but the precedence rule this test protects still
/// matters: exercises the actual `manta` subprocess (not just the internal
/// merge functions) both ways -- an explicit `--engine legacy` must beat a
/// hsmm-staged file, and with no override the file's own `hsmm` value must
/// be honored (both cases failing only for the expected, unrelated
/// downstream reason: the nonexistent source file).
#[test]
fn cli_engine_override_beats_a_hsmm_staged_server_config_file() {
    use std::io::Write as _;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(
        f,
        r#"
        [server]
        station_callsign = "W3XYZ"
        [decode]
        engine = "hsmm"
        "#
    )
    .unwrap();
    f.flush().unwrap();

    // With --engine legacy: the override must win over the file's hsmm
    // value and fail only for the expected downstream reason (source file
    // doesn't exist).
    let out = manta()
        .args(["listen", "--engine", "legacy", "--server-config"])
        .arg(f.path())
        .args(["--source", "/nonexistent.wav", "--dial-freq-hz", "14027000"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a failure (nonexistent source)"
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "--engine must not be rejected as a bad argument"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent.wav") || stderr.contains("No such file"),
        "expected the nonexistent-source-file error, got: {stderr}"
    );

    // With NO --engine override: the file's own hsmm value is honored (not
    // rejected) and the run still fails only for the same unrelated,
    // expected reason.
    let out = manta()
        .args(["listen", "--server-config"])
        .arg(f.path())
        .args(["--source", "/nonexistent.wav", "--dial-freq-hz", "14027000"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a failure (nonexistent source)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent.wav") || stderr.contains("No such file"),
        "expected the nonexistent-source-file error, got: {stderr}"
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
