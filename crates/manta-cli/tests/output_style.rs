//! Operator output contracts. Machine values are intentionally not rounded.
use std::process::{Command, Output};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("RUST_LOG");
    cmd
}

fn failure(out: &Output, code: i32) -> String {
    assert_eq!(out.status.code(), Some(code));
    assert!(out.stdout.is_empty());
    let text = String::from_utf8(out.stderr.clone()).unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(
        !text.trim_end_matches('\n').chars().any(char::is_control),
        "{text:?}"
    );
    text
}

#[test]
fn application_errors_keep_values_and_escape_controls() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["v99", "bad\u{1b}[2J\n\t  ", "quote'  "] {
        let out = manta()
            .args(["gen", name, "--out"])
            .arg(dir.path())
            .output()
            .unwrap();
        let text = failure(&out, 1);
        assert!(
            text.contains(&format!(
                "unknown vector '{}'",
                manta_server::status_doc::escape_for_terminal(name).replace('\'', "\\'")
            )),
            "{text}"
        );
    }
    let path = dir.path().join("missing  file.wav  ");
    let out = manta().arg("decode").arg(&path).output().unwrap();
    let text = failure(&out, 1);
    assert!(
        text.contains(&format!("open WAV {}: ", path.display())),
        "{text}"
    );
}

#[test]
fn toml_errors_keep_source_and_caret_on_one_physical_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "not valid toml [[[\n").unwrap();
    for (args, code) in [
        (vec!["config", "check", "--config"], 1),
        (vec!["status", "--config"], 2),
    ] {
        let out = manta().args(args).arg(&path).output().unwrap();
        let text = failure(&out, code);
        assert!(
            text.contains("TOML parse error at line 1, column 5"),
            "{text}"
        );
        assert!(
            text.contains(r"\n1 | not valid toml [[[\n  |     ^\n"),
            "{text}"
        );
    }
}

#[test]
fn clap_preserves_help_version_and_usage_exit_codes() {
    for flag in ["--help", "--version"] {
        let out = manta().arg(flag).output().unwrap();
        assert!(out.status.success());
        assert!(!out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
    for args in [
        vec!["decode"],
        vec!["run", "--dial-freq-hz", "oops"],
        vec!["run", "--capture-rate-hz", "oops"],
        vec!["run", "--replay-epoch", "oops"],
    ] {
        let out = manta().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        let text = String::from_utf8(out.stderr).unwrap();
        assert!(text.starts_with("error:"));
        if args == ["decode"] {
            assert!(text.contains("Usage:"));
        }
        if let Some(flag) = args.get(1) {
            assert!(!text.contains(&format!("invalid {flag}")), "{text}");
        }
    }
}

#[test]
fn config_inspection_keeps_quotes_exact_values_and_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[input]\ntype = 'audio'\ndevice = '  USB Audio  '\n").unwrap();
    let out = manta()
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("device=\"  USB Audio  \""));
    std::fs::write(
        &path,
        "[input]\ntype = 'kiwi'\nhost = 'localhost'\nfreq_hz = 14012349.9\npassword = 'SECRET'\n",
    )
    .unwrap();
    let out = manta()
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("freq_hz=14012349.9 password=set"));
    assert!(!text.contains("SECRET"));
    assert!(out.stderr.is_empty());
}

fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join("v1.wav")
}

#[test]
fn command_reports_own_their_streams_and_json_preserves_measurements() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path());
    let cfg = manta_engine::PipelineConfig::default();
    let report = manta_engine::decode_wav(&wav, &cfg).unwrap();
    let out = manta().arg("decode").arg(&wav).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!report.text.is_empty());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{}\n", report.text)
    );
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!(
            "frequency: {}  speed: {}  spots: {}\n",
            manta_server::human::khz(report.freq_hz),
            manta_server::human::wpm(report.wpm.unwrap()),
            report.spots.len()
        )
    );
    let out = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json,
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&report).unwrap())
            .unwrap()
    );
    assert_ne!(report.freq_hz.fract(), 0.0);
    assert_ne!(report.wpm.unwrap().fract(), 0.0);

    let out = manta()
        .args(["soak", "--source-iq", "--source"])
        .arg(&wav)
        .args(["--duration", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        regex::Regex::new(
            r"\Asoak: passed\nevents: \d+\nRSS growth: \d+\.\d MiB\npanicked: no\n\z"
        )
        .unwrap()
        .is_match(&text),
        "{text}"
    );

    let src = manta_input::WavIqSource::open(&wav).unwrap();
    let report =
        manta_engine::doctor(Box::new(src), &cfg, std::time::Duration::from_secs(10)).unwrap();
    let mut expected = serde_json::to_value(&report).unwrap();
    expected["verdict"] = serde_json::to_value(report.verdict()).unwrap();
    let out = manta()
        .args(["doctor", "--source-iq", "--source"])
        .arg(&wav)
        .args(["--duration", "10", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::from_str::<serde_json::Value>(&expected.to_string()).unwrap()
    );
    let out = manta()
        .args(["doctor", "--source-iq", "--source"])
        .arg(&wav)
        .args(["--duration", "10"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(&format!(
            "source: {:.0} Hz sample rate, 14000.0 kHz center, observed for 10.0s\n",
            report.sample_rate_hz
        )),
        "{text}"
    );
    assert!(text.contains("SNR (2500 Hz reference):"));
    assert!(text.ends_with(&format!("verdict: {}\n", report.verdict().summary())));
    if let (Some(min), Some(median), Some(max)) =
        (report.snr_db_min, report.snr_db_median, report.snr_db_max)
    {
        assert!(text.contains(&format!(
            "min={} median={} max={}",
            manta_server::human::db(min),
            manta_server::human::db(median),
            manta_server::human::db(max)
        )));
    }
}

#[test]
fn gen_confirmation_uses_human_frequency_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["gen", "v1", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!(
            "wrote {}/{{v1.wav,v1.json,v1.manifest.json}} (expected frequency 14012.3 kHz)\n",
            dir.path().display()
        )
    );
}

#[test]
fn run_spots_are_stdout_only_repeatable_and_json_stays_raw() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture(dir.path());
    let config = dir.path().join("input.toml");
    std::fs::write(
        &config,
        "[input]\ntype = 'file'\npath = 'v1.wav'\niq = true\n",
    )
    .unwrap();
    let run = || {
        manta()
            .args(["run", "--source-iq", "--source"])
            .arg(&wav)
            .output()
            .unwrap()
    };
    let a = run();
    let b = run();
    assert!(a.status.success() && b.status.success());
    assert!(!a.stdout.is_empty());
    assert_eq!(a.stdout, b.stdout);
    let stdout = String::from_utf8(a.stdout).unwrap();
    let stderr = String::from_utf8(a.stderr).unwrap();
    assert!(stdout.contains("W1AW"));
    let line = regex::Regex::new(r"^SPOT: [A-Z0-9/]+ \((CQ|DE|BEACON|unknown)\) -?\d+\.\d kHz -?\d+ dB \d+ WPM conf=\d+\.\d{2}$").unwrap();
    assert!(stdout.lines().all(|s| line.is_match(s)), "{stdout}");
    assert!(stdout.ends_with('\n'));
    assert!(stderr.starts_with("manta: listening;"));
    assert!(stderr.contains("W1AW"), "monitor absent: {stderr}");
    assert!(!stderr.contains("SPOT:"));
    assert!(stderr.ends_with('\n'));
    let alias = manta()
        .args(["listen", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(alias.status.success());
    assert_eq!(String::from_utf8(alias.stdout).unwrap(), stdout);
    assert!(String::from_utf8_lossy(&alias.stderr).contains("deprecated"));
    let out = manta()
        .args(["run", "--source-iq", "--source"])
        .arg(&wav)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let records: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let spots: Vec<_> = records.iter().filter_map(|v| v.get("spot")).collect();
    assert!(!spots.is_empty());
    assert!(records.len() > spots.len());
    let spot = spots.iter().find(|s| s["callsign"] == "W1AW").unwrap();
    assert_eq!(spot["spot_type"], "Cq");
    for key in ["freq_hz", "wpm", "snr_db", "confidence"] {
        assert_ne!(spot[key].as_f64().unwrap().fract(), 0.0, "{key}: {spot}");
    }
    assert_ne!(
        spot["confidence"].as_f64().unwrap() * 100.0,
        (spot["confidence"].as_f64().unwrap() * 100.0).round()
    );
}

#[test]
fn each_listener_bind_error_names_its_address_and_port() {
    use std::net::TcpListener;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("silence.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
    for _ in 0..144000 {
        writer.write_sample(0i16).unwrap();
    }
    writer.finalize().unwrap();
    for (listener, key, address) in [
        ("telnet", "telnet_port", "bind_addr"),
        ("JSON", "json_port", "bind_addr"),
        ("metrics", "metrics_port", "metrics_bind_addr"),
    ] {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let mut config = String::from("[server]\nstation_callsign = 'W1AW'\nbind_addr = '127.0.0.1'\nmetrics_bind_addr = '127.0.0.1'\n");
        for name in ["telnet_port", "json_port", "metrics_port"] {
            config.push_str(&format!(
                "{name} = {}\n",
                if name == key { port } else { 0 }
            ));
        }
        let path = dir.path().join("server.toml");
        std::fs::write(&path, config).unwrap();
        let out = manta()
            .args(["run", "--source"])
            .arg(&wav)
            .args(["--dial-freq-hz", "14000000", "--config"])
            .arg(&path)
            .output()
            .unwrap();
        let text = failure(&out, 1);
        assert!(
            text.contains(&format!(
                "binding the {listener} server ({address} = \"127.0.0.1\", {key} = {port})"
            )),
            "{text}"
        );
    }
}
