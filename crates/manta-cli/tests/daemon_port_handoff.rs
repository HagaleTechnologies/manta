//! MAN-570 acceptance: two daemons launched at the same time, each with
//! metrics enabled, both get distinct usable listeners -- never
//! `Address already in use`. Both bind `*_port = 0` (the kernel assigns
//! ports that are held from the moment they exist) and report them in the
//! `listening:` banner, which `spawn_daemon_reporting_ports` parses; no
//! released port number is ever handed on for the daemon to bind.

mod common;
<<<<<<< HEAD

use common::{manta, spawn_daemon_reporting_ports, DaemonHandle};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
=======
#[path = "man570/daemon.rs"]
mod daemon;

use common::manta;
use daemon::{spawn_daemon_reporting_ports, DaemonHandle};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
>>>>>>> f9ce1657d4338e65a6778fa130dc8d93b5c17d84
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

<<<<<<< HEAD
fn connect_to_daemon(port: u16) -> TcpStream {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(10))
        .unwrap_or_else(|err| panic!("connect to daemon on port {port}: {err}"))
}

/// The body of a `GET /metrics` on `port`.
fn scrape_metrics(port: u16) -> String {
    let mut stream = connect_to_daemon(port);
=======
/// The body of a `GET /metrics` on `port`.
fn scrape_metrics(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
>>>>>>> f9ce1657d4338e65a6778fa130dc8d93b5c17d84
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
<<<<<<< HEAD
    std::thread::scope(|scope| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let churn = scope.spawn(move || {
            let mut first = true;
            while matches!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                let client = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(1);
                loop {
                    match listener.accept() {
                        Ok((server, _)) => {
                            drop(server);
                            break;
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => panic!("churn accept failed: {e}"),
                    }
                }
                drop(client);
                if first {
                    ready_tx.send(()).unwrap();
                    first = false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let a = scope.spawn(|| spawn_daemon(&a_dir, &wav, "a"));
        let b = scope.spawn(|| spawn_daemon(&b_dir, &wav, "b"));
        let a = a.join().expect("daemon a spawned");
        let b = b.join().expect("daemon b spawned");
        drop(stop_tx);
        churn.join().expect("loopback churn completed");
        check_daemons(&a, &b);
    });
}

fn check_daemons(a: &DaemonHandle, b: &DaemonHandle) {
    let a_ports = (a.ports.telnet, a.ports.json, a.ports.metrics);
    let b_ports = (b.ports.telnet, b.ports.json, b.ports.metrics);
    let ports = [
        a_ports.0, a_ports.1, a_ports.2, b_ports.0, b_ports.1, b_ports.2,
    ];
    assert!(
        ports.iter().all(|port| *port != 0),
        "zero listener: {ports:?}"
    );
    assert_eq!(
        ports.into_iter().collect::<HashSet<_>>().len(),
        6,
        "duplicate listener: {ports:?}"
    );
    for port in [a.ports.telnet, a.ports.json, b.ports.telnet, b.ports.json] {
        connect_to_daemon(port);
=======
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
>>>>>>> f9ce1657d4338e65a6778fa130dc8d93b5c17d84
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
