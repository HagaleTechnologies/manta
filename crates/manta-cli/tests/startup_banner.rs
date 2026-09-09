//! MAN-122 scenario 1 + 2 acceptance.
use std::io::Write as _;

/// `seconds` of 48 kHz mono silence. The banner is emitted before the decode
/// loop starts, so the fixture's content is irrelevant here and silence keeps
/// it small/fast; its LENGTH is load-bearing only in one direction -- shorter
/// than `manta_engine`'s two-second calibration window means the pipeline
/// never starts, which is exactly what the second test wants.
fn silent_48k_wav(dir: &std::path::Path, seconds: u32) -> std::path::PathBuf {
    let path = dir.join("silence.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..(48_000 * seconds) {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn write_server_config(dir: &std::path::Path, status_interval_secs: u64) -> std::path::PathBuf {
    let cfg_path = dir.join("server.toml");
    let mut cfg = std::fs::File::create(&cfg_path).unwrap();
    cfg.write_all(
        format!(
            r#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0
            status_interval_secs = {status_interval_secs}
            "#
        )
        .as_bytes(),
    )
    .unwrap();
    drop(cfg);
    cfg_path
}

#[test]
fn the_daemon_logs_a_startup_banner_before_any_client_connects() {
    let dir = tempfile::tempdir().unwrap();
    // 3 s, not the bare minimum: manta_engine::listen's calibration phase
    // (`CALIBRATION_SECONDS = 2.0`) needs the source to yield at least 2 s
    // of samples before it will run at all -- a 2 s-exact fixture has zero
    // margin and fails on off-by-one/rounding grounds unrelated to the
    // banner this test actually verifies.
    let wav = silent_48k_wav(dir.path(), 3);
    // status_interval_secs = 0: this test only cares about the banner.
    let cfg_path = write_server_config(dir.path(), 0);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            // MAN-77 made `run --config` the canonical daemon verb;
            // the `listen`/`--server-config` aliases still work but print
            // a deprecation warning to stderr, which would sit AHEAD of
            // the banner and defeat the first-line assertion below.
            "run",
            "--source",
            wav.to_str().unwrap(),
            "--dial-freq-hz",
            "14060000",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);

    let stderr = String::from_utf8_lossy(&out.stderr);
    let banner = stderr
        .lines()
        .find(|l| l.contains("manta ") && l.contains("listening:"))
        .unwrap_or_else(|| panic!("no startup banner in stderr: {stderr}"));

    for needle in [
        env!("CARGO_PKG_VERSION"),
        "source=file",
        "sample_rate_hz=48000",
        "dial_freq_hz=14060000",
        "telnet=127.0.0.1:",
        "json=127.0.0.1:",
        "metrics=127.0.0.1:",
    ] {
        assert!(
            banner.contains(needle),
            "{needle:?} missing from {banner:?}"
        );
    }
    // Scenario 1's "CURRENTLY" note: the banner must be the FIRST line, not
    // something an operator only sees after a client happens to connect.
    assert_eq!(
        stderr.lines().next(),
        Some(banner),
        "the banner must be the first line the daemon logs"
    );
    assert!(
        !banner.contains(":0 "),
        "the banner must name the real bound port, not the configured 0: {banner:?}"
    );
    // Review round 2: the banner claims bound sockets; readiness to decode is
    // a separate, strictly later event. This fixture is longer than the
    // calibration window, so the pipeline does start and both lines appear,
    // in that order.
    let ready_at = stderr
        .lines()
        .position(|l| l.contains("ready: decoding"))
        .unwrap_or_else(|| panic!("no pipeline-ready line in stderr: {stderr}"));
    let banner_at = stderr
        .lines()
        .position(|l| l == banner)
        .expect("the banner was just found in this same stderr");
    assert!(
        banner_at < ready_at,
        "the listeners-bound banner must precede the readiness event: {stderr}"
    );
}

/// MAN-122 review round 2. The predecessor of this test asserted that a
/// periodic status line appears during an unpaced 240-second file replay --
/// which is only true while the replay takes longer in real time than the
/// status interval. That is a property of the runner, not of the daemon: a
/// fast enough machine decodes the fixture inside one interval, shutdown
/// cancels the status task before its first tick, and the test goes red on
/// correct production behaviour. Nothing in the CLI's surface can pace a
/// file replay (`AudioIqSource` loads the whole WAV eagerly and reads from
/// memory), so the timing half of scenario 2 is pinned where time IS
/// controllable -- `manta-server`'s `status_line_acceptance.rs`, against
/// the real `spawn_status_line` task, plus the interval/shutdown unit tests
/// in `manta-server::status`.
///
/// What is left here is the half that needs the real binary and cannot
/// race: readiness is not claimed for a daemon whose pipeline never
/// started, and a daemon that never decodes never emits a status line
/// either.
#[test]
fn a_daemon_whose_pipeline_never_starts_never_claims_to_be_ready() {
    let dir = tempfile::tempdir().unwrap();
    // 1 s < CALIBRATION_SECONDS (2.0): the source runs out inside the
    // calibration read, so `listen` fails before a single batch is
    // processed. Deterministic on any machine at any speed -- it is a
    // property of the fixture, not of how fast the fixture is consumed.
    let wav = silent_48k_wav(dir.path(), 1);
    let cfg_path = write_server_config(dir.path(), 1);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            // Canonical verb, as above: the deprecated `listen` alias
            // would prepend a warning line to stderr.
            "run",
            "--source",
            wav.to_str().unwrap(),
            "--dial-freq-hz",
            "14060000",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a replay shorter than the calibration window must fail, not succeed"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.lines().any(|l| l.contains("listening:")),
        "the listeners really were bound, so the banner must still be there: {stderr}"
    );
    assert!(
        !stderr.contains("ready: decoding"),
        "the daemon exited during calibration without decoding anything -- it must \
         never have claimed readiness: {stderr}"
    );
    assert!(
        !stderr.contains("manta status:"),
        "no status line can be emitted by a daemon that never got past calibration: {stderr}"
    );
}
