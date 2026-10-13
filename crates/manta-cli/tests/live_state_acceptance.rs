//! MAN-324 acceptance: a dashboard follows a node's live state. The real
//! binary, real sockets and a paced (`--realtime`) V1 replay, as in
//! `replay_acceptance.rs`.
//!
//! Scenario 1: a subscriber receives per-band state every second.
//! Scenario 2: it sees each track's decoded text and the candidates.
//! Scenario 3: it sees why a candidate was or was not spotted.
//! Scenario 4: a subscriber that stops reading never slows decoding.
//!
//! Deadlines are generous and exist only so a broken build fails cleanly
//! instead of hanging.

use std::io::{BufRead, BufReader, ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Kills the daemon if a test fails before it exits by itself.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

/// A loopback `[server]` on ephemeral ports, with the live-state listener
/// when `live` is set.
fn server_toml(dir: &Path, live: bool) -> PathBuf {
    let path = dir.join("live.toml");
    let live_line = if live { "live_port = 0\n" } else { "" };
    std::fs::write(
        &path,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             telnet_port = 0\njson_port = 0\nmetrics_port = 0\nstatus_interval_secs = 0\n\
             {live_line}"
        ),
    )
    .unwrap();
    path
}

/// `dir/v1.wav` (+ sidecar, centre 14 MHz): V1 rendered for `duration_s`.
fn v1_fixture(dir: &Path, duration_s: f64) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join("v1.wav")
}

/// The `listening:` banner line, or a panic at `deadline`.
fn banner(rx: &Receiver<String>, deadline: Instant) -> String {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains("listening:") => return line,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the banner"),
            Err(RecvTimeoutError::Disconnected) => panic!("manta exited before its banner"),
        }
    }
}

/// The port the banner names for `key` (`json`, `metrics`, `live`).
fn port_in(banner: &str, key: &str) -> Option<u16> {
    let re = regex::Regex::new(&format!(r" {key}=127\.0\.0\.1:(\d+)")).unwrap();
    re.captures(banner).map(|c| c[1].parse().unwrap())
}

/// Reads JSON lines from `stream` until one satisfies `pred`, or panics at
/// `deadline`.
fn read_json_until(
    stream: &mut BufReader<TcpStream>,
    deadline: Instant,
    what: &str,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
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
                let value: serde_json::Value = serde_json::from_str(line.trim_end())
                    .unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}"));
                line.clear();
                if pred(&value) {
                    return value;
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("read failed waiting for {what}: {e}"),
        }
    }
}

fn connect(port: u16) -> BufReader<TcpStream> {
    BufReader::new(TcpStream::connect(("127.0.0.1", port)).unwrap())
}

fn get_metrics(port: u16) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body
}

fn is_w1aw_decision(v: &serde_json::Value, decision: &str) -> bool {
    v["kind"] == "candidateDecision" && v["call"] == "W1AW" && v["decision"] == decision
}

/// Scenarios 1-3 end to end: band state, the track's text and candidates,
/// the repetition withhold and then the spotted decision whose `spotId`
/// is the `:7301` spot's `id`.
#[test]
fn a_live_client_sees_band_state_track_text_candidates_and_the_repetition_decision() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path(), 30.0);
    let cfg = server_toml(dir.path(), true);
    let spawned = Instant::now();
    let (_daemon, rx) = spawn_daemon(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--realtime".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
    ]);
    let line = banner(&rx, spawned + Duration::from_secs(30));
    let live_port = port_in(&line, "live").unwrap_or_else(|| panic!("no live= in {line:?}"));
    let json_port = port_in(&line, "json").unwrap();
    let metrics_port = port_in(&line, "metrics").unwrap();
    let mut live = connect(live_port);
    let mut spots = connect(json_port);
    let deadline = spawned + Duration::from_secs(60);

    let bands = read_json_until(&mut live, deadline, "a 20m bandState with a track", |v| {
        v["kind"] == "bandState"
            && v["deCall"] == "W1AW"
            && v["bands"].as_array().is_some_and(|b| {
                b.iter()
                    .any(|band| band["band"] == "20m" && band["activeTracks"].as_u64() >= Some(1))
            })
    });
    for band in bands["bands"].as_array().unwrap() {
        assert!(band["noiseFloorDb"].is_number(), "{bands}");
        assert!(band["occupancy"].is_number(), "{bands}");
    }
    let track = read_json_until(&mut live, deadline, "a trackState naming W1AW", |v| {
        v["kind"] == "trackState"
            && v["text"].as_str().is_some_and(|t| t.contains("W1AW"))
            && v["candidates"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| c["call"] == "W1AW"))
    });
    assert_eq!(track["band"], "20m", "{track}");
    assert!(
        track["frequency"].is_i64() && track["snrRefHz"] == 2500,
        "{track}"
    );
    let withheld = read_json_until(&mut live, deadline, "W1AW withheld", |v| {
        is_w1aw_decision(v, "withheld") || is_w1aw_decision(v, "spotted")
    });
    assert_eq!(
        withheld["decision"], "withheld",
        "the first decision withholds: {withheld}"
    );
    assert_eq!(withheld["reason"], "repetition", "{withheld}");
    assert_eq!(withheld["requiredReps"], 2, "{withheld}");
    let spotted = read_json_until(&mut live, deadline, "W1AW spotted", |v| {
        is_w1aw_decision(v, "spotted")
    });
    let spot = read_json_until(&mut spots, deadline, "the W1AW spot on :7301", |v| {
        v["dxCall"] == "W1AW"
    });
    assert_eq!(spotted["spotId"], spot["id"], "{spotted} vs {spot}");
    assert_eq!(spotted["reason"], serde_json::Value::Null);

    let metrics = get_metrics(metrics_port);
    assert!(
        metrics.contains("manta_listener_up{listener=\"live\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("manta_live_clients_connected 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("manta_live_records_total{kind=\"candidateDecision\"}"),
        "{metrics}"
    );
}

/// Scenario 4 end to end: a live client that never reads, and the spot
/// still reaches `:7301`; the daemon finishes the replay and exits 0.
#[test]
fn a_stalled_live_client_does_not_stop_the_spot() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path(), 30.0);
    let cfg = server_toml(dir.path(), true);
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
    let line = banner(&rx, spawned + Duration::from_secs(30));
    let _stalled = TcpStream::connect(("127.0.0.1", port_in(&line, "live").unwrap())).unwrap();
    let mut spots = connect(port_in(&line, "json").unwrap());
    let deadline = spawned + Duration::from_secs(60);
    read_json_until(&mut spots, deadline, "the W1AW spot on :7301", |v| {
        v["dxCall"] == "W1AW"
    });
    let exit_deadline = spawned + Duration::from_secs(90);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success(), "the daemon exited {status}");
            break;
        }
        assert!(Instant::now() < exit_deadline, "the daemon never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Without `live_port` nothing changes: no `live=` in the banner and no
/// live metric family.
#[test]
fn no_live_listener_without_live_port() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path(), 1.5);
    let cfg = server_toml(dir.path(), false);
    let spawned = Instant::now();
    let (_daemon, rx) = spawn_daemon(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--loop".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
    ]);
    let line = banner(&rx, spawned + Duration::from_secs(30));
    assert!(!line.contains("live="), "{line}");
    let metrics = get_metrics(port_in(&line, "metrics").unwrap());
    assert!(metrics.contains("manta_spots_total"), "{metrics}");
    assert!(!metrics.contains("manta_live_"), "{metrics}");
    assert!(!metrics.contains("listener=\"live\""), "{metrics}");
}
