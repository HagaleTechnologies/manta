//! MAN-122 scenario 1 + 2 acceptance.
use std::io::Write as _;

/// `seconds` of 48 kHz mono silence -- enough for the daemon to bind, log,
/// and (for the longer fixture) run past a short status interval. The
/// banner is emitted before the decode loop starts, so the fixture's
/// content is irrelevant here and silence keeps it small/fast.
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
            "listen",
            "--source",
            wav.to_str().unwrap(),
            "--dial-freq-hz",
            "14060000",
            "--server-config",
            cfg_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);

    let stderr = String::from_utf8_lossy(&out.stderr);
    let banner = stderr
        .lines()
        .find(|l| l.contains("manta ") && l.contains("ready:"))
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
}

#[test]
fn the_daemon_logs_a_periodic_status_line_while_it_runs() {
    // File replay is unpaced -- there is no way to hold a file-backed daemon
    // open for a fixed wall time, so fixture length IS the timing margin:
    // decode must take longer in real time than status_interval_secs, or
    // shutdown races the status task's sleep and no line ever fires
    // (confirmed live -- a 30 s fixture decodes in ~0.45s, well under the
    // 1 s interval below, and the test fails). 240 s of 48 kHz silence
    // gives ample margin for at least one status line to fire even on a
    // slow machine.
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 240);
    let cfg_path = write_server_config(dir.path(), 1);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            "listen",
            "--source",
            wav.to_str().unwrap(),
            "--dial-freq-hz",
            "14060000",
            "--server-config",
            cfg_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);

    let stderr = String::from_utf8_lossy(&out.stderr);
    let status_lines = stderr
        .lines()
        .filter(|l| l.contains("manta status:"))
        .count();
    assert!(status_lines >= 1, "no status line in {stderr}");
}
