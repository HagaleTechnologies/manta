//! MAN-130: manta's output must read like a finished product, never a raw
//! Rust Debug rendering, and every frequency/SNR/WPM/confidence value must
//! use one consistent style across every command and output mode.

use std::process::Command;

fn manta() -> Command {
    Command::new(env!("CARGO_BIN_EXE_manta"))
}

#[test]
fn a_missing_file_is_one_lowercase_error_line() {
    let out = manta()
        .args(["decode", "/nonexistent/nope.wav"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr.starts_with("error: open WAV /nonexistent/nope.wav: "),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Caused by:"),
        "multi-line anyhow chain leaked: {stderr}"
    );
    assert_eq!(
        stderr.lines().filter(|l| !l.is_empty()).count(),
        1,
        "{stderr}"
    );
}

#[test]
fn a_wrong_rate_source_gets_an_error_and_a_hint() {
    // v1.wav is a 96 kHz IQ fixture; --source wants 48 kHz mono audio.
    let dir = tempfile::tempdir().unwrap();
    manta_testkit::vectors::write_fixture_set(
        &manta_testkit::vectors::VectorSpec {
            duration_s: 2.0,
            ..manta_testkit::vectors::v1()
        },
        dir.path(),
    )
    .unwrap();
    let out = manta()
        .arg("listen")
        .arg("--source")
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error: "), "{stderr}");
    assert!(
        stderr.contains("\nhint: "),
        "expected a hint line, got: {stderr}"
    );
}

#[test]
fn an_unknown_vector_is_not_debug_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["gen", "v99", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown vector 'v99'"), "{stderr}");
    assert!(
        !stderr.contains("\"v99\""),
        "Debug-quoted string leaked: {stderr}"
    );
}

#[test]
fn clap_usage_errors_still_exit_2_and_stay_lowercase() {
    let out = manta().arg("decode").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

#[test]
fn a_bad_flag_value_is_not_reported_twice() {
    let out = manta()
        .args(["decode", "x.wav", "--freq-correction-ppm", "abc"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("freq-correction-ppm").count(), 1, "{stderr}");
    assert!(
        !stderr.contains("\"abc\""),
        "Debug-quoted value leaked: {stderr}"
    );
}

/// The range-check branch of a value parser must not repeat what clap's own
/// frame already prints -- the parse-failure branch was de-duplicated first,
/// the range branch second (MAN-130 remediation).
#[test]
fn an_out_of_range_flag_value_is_not_reported_twice() {
    let out = manta()
        .args(["listen", "--source", "x.wav", "--dial-freq-hz", "0"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("--dial-freq-hz").count(), 1, "{stderr}");
    assert_eq!(stderr.matches("got 0").count(), 0, "{stderr}");
}

/// A mistyped `--source` path names the file it could not open and gets NO
/// sample-rate hint: the hint is about a file's contents, and there is no
/// file (MAN-130 remediation).
#[test]
fn a_missing_source_names_the_path_and_skips_the_rate_hint() {
    let out = manta()
        .arg("listen")
        .args(["--source", "/nonexistent/nope.wav"])
        .args(["--dial-freq-hz", "14000000"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: open audio source /nonexistent/nope.wav: "),
        "{stderr}"
    );
    assert!(
        !stderr.contains("hint: "),
        "48 kHz hint fired on a path that does not exist: {stderr}"
    );
}

#[test]
fn decode_summary_has_no_option_and_reads_in_khz() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let out = manta()
        .arg("decode")
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("Some("), "Option Debug leaked: {stderr}");
    assert!(!stderr.contains("None"), "Option Debug leaked: {stderr}");
    // frequency reads in kHz to one decimal, e.g. "frequency: 14012.3 kHz"
    let re =
        regex::Regex::new(r"frequency: \d+\.\d kHz  speed: (\d+|unknown) wpm  spots: \d+").unwrap();
    assert!(re.is_match(&stderr), "{stderr}");
}

/// Writes a mono 48 kHz real-audio WAV keying `text` at `wpm` with a
/// channel-centre tone (750 Hz = 8 * 93.75 Hz channel spacing, avoiding the
/// near-channel-edge decode degradation the same choice
/// `manta-engine/tests/listen_audio.rs` documents) plus a low, seeded AWGN
/// floor -- a *loud* (0.30 FS, zero-noise) scene decodes to noise rather
/// than spotting, while amplitude 0.05 FS with sigma 0.005 noise decodes
/// and spots reliably.
fn write_48k_cw_wav(path: &std::path::Path, text: &str, wpm: f32, extra_secs: f64) {
    let fs = 48_000.0;
    let spec = manta_testkit::keyer::KeyerSpec::new(wpm);
    let (env, _) = manta_testkit::keyer::key_text_loop(text, &spec, fs, extra_secs).unwrap();

    // `add_unit_awgn`'s real component has variance 0.5 (sigma ~0.7071);
    // scale to the target noise sigma (0.005) rather than pull in a new
    // noise-distribution dependency for one test fixture.
    let mut noise = vec![num_complex::Complex32::new(0.0, 0.0); env.len()];
    manta_testkit::noise::add_unit_awgn(&mut noise, 0xA5A5_A5A5);
    let noise_scale = 0.005 / 0.5f32.sqrt();

    let dphi = std::f64::consts::TAU * 750.0 / fs;
    let mut phi = 0.0f64;
    let samples: Vec<f32> = env
        .iter()
        .zip(noise.iter())
        .map(|(&e, n)| {
            let tone = e * phi.cos() as f32 * 0.05;
            phi += dphi;
            tone + n.re * noise_scale
        })
        .collect();

    let wav_spec = hound::WavSpec {
        channels: 1,
        sample_rate: fs as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, wav_spec).unwrap();
    for s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn soak_prints_a_human_report_on_stdout_not_a_debug_struct() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("cw48k.wav");
    write_48k_cw_wav(&wav, "CQ CQ DE W1AW W1AW K", 20.0, 8.0);
    let out = manta()
        .args(["soak", "--duration", "5", "--source"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("SoakReport"),
        "Debug struct leaked: {stdout}"
    );
    assert!(stdout.starts_with("soak: passed"), "{stdout}");
    assert!(stdout.contains("panicked:    no"), "{stdout}");
}

#[test]
fn soak_json_is_one_object_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("cw48k.wav");
    write_48k_cw_wav(&wav, "CQ CQ DE W1AW W1AW K", 20.0, 8.0);
    let out = manta()
        .args(["soak", "--duration", "5", "--json", "--source"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["passed"], true);
    assert_eq!(v["panicked"], false);
    assert!(v["events_emitted"].as_u64().unwrap() > 0);
}

#[test]
fn listen_text_mode_puts_only_spot_lines_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("cw48k.wav");
    write_48k_cw_wav(&wav, "CQ CQ DE W1AW W1AW K", 20.0, 40.0);
    let out = manta()
        .arg("listen")
        .arg("--source")
        .arg(&wav)
        .args(["--dial-freq-hz", "14000000"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The spot-type alternatives are spelled with their padding: the column
    // is `{:<7}`, so every label occupies exactly seven characters and the
    // `conf` column lines up across CQ/DE/BEACON/unknown rows. A loose
    // `\S*\s*` here matched with or without the padding, which is how a
    // `Display` that ignored the width spec once passed this tier (MAN-130).
    let spot = regex::Regex::new(
        r"^\s*\d+\.\d kHz  \S+\s* CW  \s*-?\d+ dB  \s*\d+ WPM  (?:CQ     |DE     |BEACON |unknown) conf \d\.\d\d$",
    )
    .unwrap();
    assert!(
        !stdout.trim().is_empty(),
        "expected at least one spot on stdout, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in stdout.lines() {
        assert!(spot.is_match(line), "non-spot output on stdout: {line:?}");
    }
    assert!(stdout.contains("W1AW"), "{stdout}");
    assert!(!stdout.contains("(Cq)"), "Debug enum leaked: {stdout}");
    // the live character monitor is a diagnostic
    assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
}

#[test]
fn listen_json_mode_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("cw48k.wav");
    write_48k_cw_wav(&wav, "CQ CQ DE W1AW W1AW K", 20.0, 40.0);
    let out = manta()
        .arg("listen")
        .arg("--source")
        .arg(&wav)
        .args(["--dial-freq-hz", "14000000", "--json"])
        .output()
        .unwrap();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("non-JSON on stdout in --json mode: {line:?} ({e})"));
    }
}

/// MAN-130: no `{:?}`/`{:#?}` in an operator-facing print macro, ever again.
/// Scans the CLI's own source rather than its behaviour, because a leak on
/// a rarely-taken branch would otherwise never be exercised by a
/// behavioural test.
#[test]
fn no_debug_formatting_in_any_cli_print_macro() {
    let macro_call =
        regex::Regex::new(r#"(?s)\b(?:e?print(?:ln)?!|write(?:ln)?!)\s*\(([^;]*?)\)\s*;"#).unwrap();
    let debug_spec = regex::Regex::new(r"\{[^{}]*:#?\?\}").unwrap();

    for (name, src) in [
        ("main.rs", include_str!("../src/main.rs")),
        ("fmt.rs", include_str!("../src/fmt.rs")),
    ] {
        // Drop the in-file test module: an assertion message may
        // legitimately use `{:?}`. Panic loudly if the marker moves, so the
        // guard cannot silently start scanning nothing.
        let marker = "#[cfg(test)]\nmod tests {";
        let body = match src.find(marker) {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(body.len() > src.len() / 2, "{name}: cfg(test) marker moved");

        for m in macro_call.captures_iter(body) {
            let args = &m[1];
            assert!(
                !debug_spec.is_match(args),
                "{name}: Debug rendering leaked into a print macro: {args}"
            );
        }
    }
}

#[test]
fn no_subcommands_help_or_result_output_contains_a_rust_debug_rendering() {
    for args in [
        vec!["--help"],
        vec!["decode", "--help"],
        vec!["gen", "--help"],
        vec!["listen", "--help"],
        vec!["soak", "--help"],
    ] {
        let out = manta().args(&args).output().unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for banned in ["Some(", "SoakReport {", "SpotType::"] {
            assert!(!all.contains(banned), "{args:?} leaked {banned:?}: {all}");
        }
    }
}
