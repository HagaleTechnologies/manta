//! MAN-570 acceptance: two daemons launched at the same time, each with
//! metrics enabled, both get distinct usable listeners -- never
//! `Address already in use`. Both bind `*_port = 0` (the kernel assigns
//! ports that are held from the moment they exist) and report them in the
//! `listening:` banner, which `spawn_daemon_reporting_ports` parses; no
//! released port number is ever handed on for the daemon to bind.

mod common;

use common::{manta, spawn_daemon_reporting_ports, DaemonHandle};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// A short, quiet 48 kHz mono WAV: enough input for the daemon to open
/// its source, bind its listeners and print its banner. Decoding silence
/// finds nothing, which this test does not care about.
fn quiet_wav(dir: &Path) -> PathBuf {
    let path = dir.join("quiet48k.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..48_000 * 5 {
        writer.write_sample(0i16).unwrap();
    }
    writer.finalize().unwrap();
    path
}

/// One `manta run --loop` daemon on an all-0 `[server]` config, launched
/// through the shared banner-discovery helper.
fn spawn_daemon(dir: &Path, wav: &Path, name: &str) -> DaemonHandle {
    let cfg = dir.join(format!("{name}.toml"));
    std::fs::write(
        &cfg,
        "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
         telnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
    )
    .unwrap();
    let mut cmd = manta();
    cmd.args(["run", "--source"])
        .arg(wav)
        .args(["--dial-freq-hz", "14025000", "--loop", "--config"])
        .arg(&cfg)
        .stdout(Stdio::null());
    spawn_daemon_reporting_ports(cmd, Duration::from_secs(30))
}

/// The body of a `GET /metrics` on `port`.
fn scrape_metrics(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    body
}

/// Gherkin (MAN-570):
///   Given two daemons that each launch with metrics enabled
///   When they run at the same time
///   Then neither fails with `Address already in use`
///   And both report distinct, usable listeners
#[test]
fn two_simultaneous_daemons_get_distinct_reported_ports() {
    let dir = tempfile::tempdir().unwrap();
    let wav = quiet_wav(dir.path());
    let (a_dir, b_dir) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir(&a_dir).unwrap();
    std::fs::create_dir(&b_dir).unwrap();
    // Genuinely concurrent: each spawn blocks until its own daemon's
    // banner lands, so both daemons bind while the other is starting.
    let a = {
        let (dir, wav) = (a_dir, wav.clone());
        std::thread::spawn(move || spawn_daemon(&dir, &wav, "a"))
    };
    let b = {
        let (dir, wav) = (b_dir, wav.clone());
        std::thread::spawn(move || spawn_daemon(&dir, &wav, "b"))
    };
    let (a, b) = (
        a.join().expect("daemon a spawned"),
        b.join().expect("daemon b spawned"),
    );

    let a_ports = (a.ports.telnet, a.ports.json, a.ports.metrics);
    let b_ports = (b.ports.telnet, b.ports.json, b.ports.metrics);
    // Distinct per listener, not merely as a triple: equal metrics ports
    // with one other port differing would pass a triple comparison and
    // both scrapes below could hit the same daemon (the round-2 proof
    // gap).
    assert_ne!(
        a.ports.metrics, b.ports.metrics,
        "the daemons share a metrics listener"
    );
    assert_ne!(
        a.ports.telnet, b.ports.telnet,
        "the daemons share a telnet listener"
    );
    assert_ne!(
        a.ports.json, b.ports.json,
        "the daemons share a JSON listener"
    );
    for (name, ports) in [("a", a_ports), ("b", b_ports)] {
        assert!(
            ports.0 != 0 && ports.1 != 0 && ports.2 != 0,
            "{name}: {ports:?}"
        );
    }

    // Both metrics endpoints serve, on the ports each banner reported.
    for daemon in [&a, &b] {
        let body = scrape_metrics(daemon.ports.metrics);
        assert!(
            body.contains("HTTP/1.1 200"),
            "no metrics reply on {}: {body}",
            daemon.ports.metrics
        );
        assert!(
            body.contains("manta_uptime_seconds"),
            "no uptime sample on {}: {body}",
            daemon.ports.metrics
        );
    }
}
