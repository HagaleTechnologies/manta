//! MAN-269 acceptance: a hardware-free file replay reaches a connected
//! telnet client. The ticket's two Gherkin scenarios, against the real
//! binary with real sockets and `manta gen`-shaped V1 fixtures.
//!
//! Every timing assertion is a lower bound, computed from the fixture or
//! measured in the test. Deadlines are generous and exist only so a broken
//! build fails cleanly instead of hanging.

use std::io::{BufRead, BufReader, ErrorKind, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Kills the daemon if a test fails before stopping it itself.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl KillOnDrop {
    fn still_running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

/// `manta` with no `MANTA_*` variable from the test runner's environment.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Starts `manta <args>` and forwards each stderr line, ANSI colour codes
/// stripped, over the returned channel.
fn spawn_daemon(args: &[&std::ffi::OsStr]) -> (KillOnDrop, Receiver<String>) {
    let mut child = manta()
        .args(args)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if tx.send(ansi.replace_all(&line, "").into_owned()).is_err() {
                return;
            }
        }
    });
    (KillOnDrop(child), rx)
}

/// A loopback `[server]` on ephemeral ports (the banner names the real ones).
fn server_toml(dir: &Path, status_interval_secs: u64) -> PathBuf {
    let path = dir.join("demo.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nstation_callsign = \"W5AU\"\nbind_addr = \"127.0.0.1\"\n\
             telnet_port = 0\njson_port = 0\nmetrics_port = 0\n\
             status_interval_secs = {status_interval_secs}\n"
        ),
    )
    .unwrap();
    path
}

/// `dir/v1.wav` (+ sidecar, centre 14 MHz): V1 rendered for `duration_s`.
fn v1_fixture(dir: &Path, duration_s: f64) -> (PathBuf, manta_testkit::vectors::Manifest) {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    (dir.join("v1.wav"), manifest)
}

/// The first stderr line satisfying `pred`, or a panic at `deadline`.
fn stderr_line_until(
    rx: &Receiver<String>,
    deadline: Instant,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if pred(&line) => return line,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what} on stderr"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("manta exited before logging {what}")
            }
        }
    }
}

fn telnet_port_from_banner(rx: &Receiver<String>, deadline: Instant) -> u16 {
    let banner = stderr_line_until(rx, deadline, "the listening: banner", |l| {
        l.contains("listening:")
    });
    let re = regex::Regex::new(r"telnet=127\.0\.0\.1:(\d+)").unwrap();
    re.captures(&banner)
        .unwrap_or_else(|| panic!("no telnet address in {banner:?}"))[1]
        .parse()
        .unwrap()
}

/// Reads lines from `stream` until one satisfies `pred`, or panics at
/// `deadline`.
fn read_line_until(
    stream: &mut BufReader<TcpStream>,
    deadline: Instant,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    let mut line = String::new();
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        match stream.read_line(&mut line) {
            Ok(0) => panic!("the server closed the connection before {what}"),
            Ok(_) if line.ends_with('\n') => {
                let done = line.trim_end().to_string();
                line.clear();
                if pred(&done) {
                    return done;
                }
            }
            // A partial line: keep what was read and wait for the rest.
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("telnet read failed waiting for {what}: {e}"),
        }
    }
}

/// Connects to the telnet port and logs in as `K1TEST`.
fn telnet_login(port: u16, deadline: Instant) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut reader = BufReader::new(stream);
    read_line_until(&mut reader, deadline, "the callsign prompt", |l| {
        l.contains("Please enter your callsign")
    });
    reader.get_mut().write_all(b"K1TEST\r\n").unwrap();
    reader
}

/// Recording seconds at which an unpaced, serverless replay of `wav` emits
/// its first spot: measured, so the test does not pin a decoder revision.
fn first_spot_seconds(wav: &Path, fs: f64) -> f64 {
    let out = manta()
        .args(["run", "--source"])
        .arg(wav)
        .args(["--source-iq", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "unpaced replay failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sample_ts = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("spot")?.get("sample_ts")?.as_u64())
        .expect("the fixture's unpaced replay emits no spot");
    sample_ts as f64 / fs
}

fn sleep_until(t: Instant) {
    std::thread::sleep(t.saturating_duration_since(Instant::now()));
}

/// Scenario 1: a file replay is paced so a client can connect and see a spot.
#[test]
fn a_client_connecting_five_seconds_into_a_paced_replay_receives_a_well_formed_spot() {
    let dir = tempfile::tempdir().unwrap();
    // 30 s of V1: its W1AW CQ spot lands about 21 s in, ~9 s before EOF.
    let (wav, manifest) = v1_fixture(dir.path(), 30.0);
    let spot_s = first_spot_seconds(&wav, manifest.fs);
    assert!(
        spot_s > 10.0,
        "the fixture spots too early ({spot_s:.2} s) for a client connecting at 5 s to see it live"
    );
    let cfg = server_toml(dir.path(), 0);

    let spawned = Instant::now();
    let (mut daemon, rx) = spawn_daemon(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--realtime".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
    ]);
    let port = telnet_port_from_banner(&rx, spawned + Duration::from_secs(30));

    sleep_until(spawned + Duration::from_secs(5));
    assert!(
        daemon.still_running(),
        "the daemon exited before a client five seconds in could connect"
    );
    let deadline = spawned + Duration::from_secs(90);
    let mut telnet = telnet_login(port, deadline);
    let line = read_line_until(&mut telnet, deadline, "a DX de spot", |l| {
        l.starts_with("DX de ")
    });
    let arrived = spawned.elapsed();

    let re = regex::Regex::new(
        r"^DX de W5AU-#:\s+(\d+\.\d{2})\s+W1AW\s+CW\s+-?\d+ dB\s+\d+ WPM\s+CQ\s+(\d{4}Z)$",
    )
    .unwrap();
    let caps = re
        .captures(&line)
        .unwrap_or_else(|| panic!("not a well-formed spot line: {line:?}"));
    assert_eq!(line.find("CW").unwrap() + 1, 42, "line: {line:?}");
    assert_eq!(caps.get(2).unwrap().start() + 1, 71, "line: {line:?}");
    let khz: f64 = caps[1].parse().unwrap();
    assert!(
        (khz - manifest.expected_freq_hz / 1000.0).abs() < 0.2,
        "spot at {khz} kHz, expected about {} kHz",
        manifest.expected_freq_hz / 1000.0
    );
    // Paced, not merely fast: the spot cannot reach the wire before its own
    // recording time has elapsed.
    assert!(
        arrived >= Duration::from_secs_f64(spot_s - 0.5),
        "the spot arrived at {arrived:?}, before its recording time {spot_s:.2} s"
    );
    assert!(
        daemon.still_running(),
        "the spot must arrive before the replay ends"
    );
}

#[test]
fn a_recording_shorter_than_the_calibration_window_does_not_start_without_loop() {
    // The control for the next test: without --loop, 1.5 s of V1 cannot even
    // fill the 2 s calibration window.
    let dir = tempfile::tempdir().unwrap();
    let (wav, _) = v1_fixture(dir.path(), 1.5);
    let out = manta()
        .args(["run", "--source"])
        .arg(&wav)
        .arg("--source-iq")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("audio source ended during startup calibration"),
        "stderr: {stderr}"
    );
}

/// Scenario 2: an operator can keep a short replay running for a demo.
#[test]
fn a_looped_replay_shorter_than_the_connect_time_keeps_the_server_up_until_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, _) = v1_fixture(dir.path(), 1.5);
    let cfg = server_toml(dir.path(), 1);

    let spawned = Instant::now();
    let (mut daemon, rx) = spawn_daemon(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--loop".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
    ]);
    let port = telnet_port_from_banner(&rx, spawned + Duration::from_secs(30));

    // Five seconds is past three natural ends of the recording.
    sleep_until(spawned + Duration::from_secs(5));
    assert!(daemon.still_running(), "a looped replay exited on its own");
    let _telnet = telnet_login(port, spawned + Duration::from_secs(30));

    // Decode is still progressing past four passes, not idling after an EOF.
    let uptime = regex::Regex::new(r"manta status: uptime_s=(\d+) pipeline=decoding").unwrap();
    stderr_line_until(
        &rx,
        spawned + Duration::from_secs(60),
        "a decoding status line at uptime_s >= 6",
        |l| {
            uptime
                .captures(l)
                .is_some_and(|c| c[1].parse::<u64>().unwrap() >= 6)
        },
    );
    assert!(daemon.still_running());

    // The operator stops it.
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(daemon.0.id()).unwrap();
        // SAFETY: `pid` is our own still-running child.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
        let stop_deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < stop_deadline,
                "the looped replay did not stop within 15 s of SIGINT"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "SIGINT exit status: {status:?}");
    }
    #[cfg(not(unix))]
    {
        daemon.0.kill().unwrap();
    }
}
