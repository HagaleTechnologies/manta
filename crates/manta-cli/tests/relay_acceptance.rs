//! MAN-320 acceptance on the real binary: `manta run` with a
//! `[[skimmer_relay]]` or `[[wsjtx_relay]]` and a live `[[rbn_uplink]]`
//! forwards the relayed spot to a mock collector. The daemon decodes a
//! never-ending fake KiwiSDR stream (`common::fake_kiwi_streaming`), so it
//! runs until killed; every port is 0 or test-bound.
#![cfg(unix)]

mod common;

use common::{expect_line, fake_kiwi_streaming, line_channel, KillOnDrop};
use std::io::{BufRead, BufReader, Write as _};
use std::net::{TcpListener, UdpSocket};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const LINE_TIMEOUT: Duration = Duration::from_secs(60);

fn config_toml(collector_port: u16, kiwi_port: u16, relay: &str) -> String {
    config_toml_with(collector_port, kiwi_port, relay, false)
}

fn config_toml_with(collector_port: u16, kiwi_port: u16, relay: &str, dry_run: bool) -> String {
    format!(
        "[server]\n\
         station_callsign = \"W3XYZ\"\n\
         operator_grid = \"FN42ab\"\n\
         bind_addr = \"127.0.0.1\"\n\
         metrics_bind_addr = \"127.0.0.1\"\n\
         telnet_port = 0\n\
         json_port = 0\n\
         metrics_port = 0\n\
         status_interval_secs = 0\n\
         \n[[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\n\
         target_port = {collector_port}\ndry_run = {dry_run}\n\
         {relay}\n\
         [input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {kiwi_port}\n\
         freq_hz = 14025000.0\n"
    )
}

/// A mock RBN collector: prompts, reads the login line and reports it,
/// then reports every later line.
fn mock_collector(listener: TcpListener) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        stream.write_all(b"Please enter your call:\r\n").ok();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

fn start_daemon(dir: &Path, config: &str) -> (KillOnDrop, mpsc::Receiver<String>) {
    let path = dir.join("manta.toml");
    std::fs::write(&path, config).unwrap();
    let mut daemon = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--config"])
            .arg(&path)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = line_channel(daemon.0.stderr.take().unwrap());
    (daemon, stderr)
}

fn kiwi() -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let done = done.clone();
        move || fake_kiwi_streaming(listener, done)
    });
    (port, done)
}

fn recv_matching(rx: &mpsc::Receiver<String>, needle: &str) -> String {
    let deadline = std::time::Instant::now() + LINE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains(needle) => return line,
            Ok(_) => {}
            Err(e) => panic!("no collector line containing {needle:?}: {e}"),
        }
    }
}

/// A mock CW Skimmer: greets with an unterminated login prompt, reports
/// the login line, waits for the test's go-ahead, then sends one spot.
fn mock_skimmer() -> (u16, mpsc::Receiver<String>, mpsc::Sender<()>) {
    let skimmer = TcpListener::bind("127.0.0.1:0").unwrap();
    let skimmer_port = skimmer.local_addr().unwrap().port();
    let (login_tx, login_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = skimmer.accept() else {
            return;
        };
        stream
            .write_all(b"CW Skimmer Telnet Server\r\nPlease enter your callsign: ")
            .ok();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut login = String::new();
        reader.read_line(&mut login).ok();
        login_tx.send(login.trim_end().to_string()).ok();
        // Wait until the uplink has logged in, so the relayed spot has a
        // subscriber (spots published while no uplink is connected are lost).
        go_rx.recv_timeout(LINE_TIMEOUT).ok();
        stream
            .write_all(
                b"DX de W3XYZ-2-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      0312Z\r\n",
            )
            .ok();
        std::thread::sleep(LINE_TIMEOUT);
    });
    (skimmer_port, login_rx, go_tx)
}

/// MAN-320 Scenario 1: Given a second CW skimmer serving spots by telnet on
/// the LAN, When it is added to manta as a secondary source, Then its spots
/// are forwarded through manta's uplink under the secondary's callsign.
#[test]
fn a_second_skimmers_spots_are_forwarded_under_its_callsign() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let collector_port = collector.local_addr().unwrap().port();
    let collected = mock_collector(collector);
    let (skimmer_port, login_rx, go_tx) = mock_skimmer();
    let (kiwi_port, done) = kiwi();
    let dir = tempfile::tempdir().unwrap();
    let relay = format!(
        "\n[[skimmer_relay]]\nhost = \"127.0.0.1\"\nport = {skimmer_port}\ncallsign = \"W3XYZ-2\"\n"
    );
    let (_daemon, _stderr) =
        start_daemon(dir.path(), &config_toml(collector_port, kiwi_port, &relay));

    assert_eq!(collected.recv_timeout(LINE_TIMEOUT).unwrap(), "W3XYZ");
    assert_eq!(login_rx.recv_timeout(LINE_TIMEOUT).unwrap(), "W3XYZ-2");
    go_tx.send(()).unwrap();
    let line = recv_matching(&collected, "JA1ABC");
    assert!(
        line.starts_with("DX de W3XYZ-2-#: 14027.10 JA1ABC"),
        "{line}"
    );
    assert!(line.contains("CW    30 dB  28 WPM  CQ"), "{line}");
    done.store(true, Ordering::Relaxed);
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn header(kind: u32) -> Self {
        let mut w = Writer::default();
        w.u32(0xadbc_cbda).u32(3).u32(kind);
        w
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    fn utf8(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
}

/// MAN-320 Scenario 2: Given WSJT-X is sending decodes by UDP to manta,
/// When a decode arrives, Then it is forwarded as a digital-mode spot.
#[test]
fn a_wsjtx_decode_is_forwarded_as_a_digital_spot() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let collector_port = collector.local_addr().unwrap().port();
    let collected = mock_collector(collector);
    let (kiwi_port, done) = kiwi();
    let dir = tempfile::tempdir().unwrap();
    let relay = "\n[[wsjtx_relay]]\nlisten = \"127.0.0.1:0\"\ncallsign = \"W3XYZ\"\n";
    let (_daemon, stderr) =
        start_daemon(dir.path(), &config_toml(collector_port, kiwi_port, relay));

    let listening = expect_line(
        &stderr,
        &["wsjtx_relay 127.0.0.1:", "listening"],
        LINE_TIMEOUT,
    );
    let port: u16 = listening
        .split("wsjtx_relay 127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in {listening:?}"));
    assert_eq!(collected.recv_timeout(LINE_TIMEOUT).unwrap(), "W3XYZ");

    let mut status = Writer::header(1);
    status
        .utf8("WSJT-X")
        .bytes(&14_074_000u64.to_be_bytes())
        .utf8("FT8");
    let mut decode = Writer::header(2);
    decode
        .utf8("WSJT-X")
        .bytes(&[1])
        .u32(45_000_000)
        .bytes(&(-12i32).to_be_bytes())
        .bytes(&0.1f64.to_be_bytes())
        .u32(1520)
        .utf8("~")
        .utf8("CQ K1ABC FN42")
        .bytes(&[0, 0]);
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    sender.send_to(&status.0, ("127.0.0.1", port)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    sender.send_to(&decode.0, ("127.0.0.1", port)).unwrap();

    let line = recv_matching(&collected, "K1ABC");
    assert!(
        line.starts_with("DX de W3XYZ-#:  14075.52  K1ABC          FT8   -12 dB         CQ      "),
        "{line}"
    );
    assert!(!line.contains("WPM"), "{line}");
    done.store(true, Ordering::Relaxed);
}

/// Relayed spots respect the uplink's `dry_run`: logged in, nothing sent.
#[test]
fn relayed_spots_respect_dry_run() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let collector_port = collector.local_addr().unwrap().port();
    let collected = mock_collector(collector);
    let (skimmer_port, login_rx, go_tx) = mock_skimmer();
    let (kiwi_port, done) = kiwi();
    let dir = tempfile::tempdir().unwrap();
    let relay = format!(
        "\n[[skimmer_relay]]\nhost = \"127.0.0.1\"\nport = {skimmer_port}\ncallsign = \"W3XYZ-2\"\n"
    );
    let (_daemon, _stderr) = start_daemon(
        dir.path(),
        &config_toml_with(collector_port, kiwi_port, &relay, true),
    );

    assert_eq!(collected.recv_timeout(LINE_TIMEOUT).unwrap(), "W3XYZ");
    assert_eq!(login_rx.recv_timeout(LINE_TIMEOUT).unwrap(), "W3XYZ-2");
    go_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        if let Ok(line) = collected.recv_timeout(left) {
            assert!(!line.contains("JA1ABC"), "dry-run sent {line}");
        }
    }
    done.store(true, Ordering::Relaxed);
}
