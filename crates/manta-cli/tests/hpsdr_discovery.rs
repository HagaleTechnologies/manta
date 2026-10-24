//! MAN-306 acceptance through the real daemon and a broadcast-aware Metis fake.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAC_A: [u8; 6] = [2, 17, 34, 51, 68, 85];
const MAC_B: [u8; 6] = [2, 187, 187, 187, 187, 2];

struct FakeDiscoveryRadio {
    port: u16,
    stop: Arc<AtomicBool>,
    discoveries: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeDiscoveryRadio {
    fn new(macs: Vec<[u8; 6]>) -> Self {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let discoveries = Arc::new(AtomicUsize::new(0));
        let stop_thread = stop.clone();
        let count_thread = discoveries.clone();
        let handle = thread::spawn(move || {
            let mut buf = [0; 2048];
            let mut client = None;
            let mut seq = 0u32;
            while !stop_thread.load(Ordering::Relaxed) {
                if let Ok((n, addr)) = socket.recv_from(&mut buf) {
                    if n == 63 && buf[..3] == [0xef, 0xfe, 2] {
                        count_thread.fetch_add(1, Ordering::Relaxed);
                        for mac in &macs {
                            let mut reply = [0; 60];
                            reply[..3].copy_from_slice(&[0xef, 0xfe, 2]);
                            reply[3..9].copy_from_slice(mac);
                            reply[9] = 43;
                            reply[10] = 6;
                            socket.send_to(&reply, addr).unwrap();
                        }
                    } else if n == 60 && buf[..4] == [0xef, 0xfe, 4, 1] {
                        client = Some(addr);
                    }
                }
                if let Some(addr) = client {
                    // Two USB frames, one receiver, 63 IQ samples per frame.
                    let mut packet = [0; 1032];
                    packet[..4].copy_from_slice(&[0xef, 0xfe, 1, 6]);
                    packet[4..8].copy_from_slice(&seq.to_be_bytes());
                    seq = seq.wrapping_add(1);
                    for frame in [8, 520] {
                        packet[frame..frame + 3].fill(0x7f);
                        for sample in 0..63 {
                            packet[frame + 8 + sample * 8] = 8;
                            packet[frame + 11 + sample * 8] = 8;
                        }
                    }
                    let _ = socket.send_to(&packet, addr);
                }
            }
        });
        Self {
            port,
            stop,
            discoveries,
            handle: Some(handle),
        }
    }
}

impl Drop for FakeDiscoveryRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn manta() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            command.env_remove(key);
        }
    }
    command
}

fn configured_command(
    port: u16,
    flags: bool,
    mac: Option<&str>,
    metrics: u16,
) -> (tempfile::NamedTempFile, Command) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = {metrics}\n").unwrap();
    let mut command = manta();
    command.args(["run", "--config"]).arg(file.path());
    if flags {
        command.args([
            "--hpsdr-freq-hz",
            "7030000",
            "--hpsdr-rate-hz",
            "192000",
            "--hpsdr-port",
            &port.to_string(),
        ]);
        if let Some(mac) = mac {
            command.args(["--hpsdr-mac", mac]);
        }
    } else {
        write!(
            file,
            "[input]\ntype = \"hpsdr\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\nport = {port}\n"
        )
        .unwrap();
        if let Some(mac) = mac {
            writeln!(file, "mac = \"{mac}\"").unwrap();
        }
    }
    file.flush().unwrap();
    (file, command)
}

fn healthy(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    if stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    body.lines()
        .any(|line| line == "manta_source_health{source=\"hpsdr\"} 1")
}

fn assert_streams(macs: Vec<[u8; 6]>, flags: bool, mac: Option<&str>) {
    let radio = FakeDiscoveryRadio::new(macs);
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let metrics = held.local_addr().unwrap().port();
    let (_file, mut command) = configured_command(radio.port, flags, mac, metrics);
    drop(held);
    let stderr = tempfile::tempfile().unwrap();
    let mut child = ChildGuard(
        command
            .stdout(Stdio::null())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if healthy(metrics) {
            break;
        }
        let exited = child.0.try_wait().unwrap();
        if exited.is_some() || Instant::now() >= deadline {
            use std::io::{Seek, SeekFrom};
            let mut stderr = stderr;
            stderr.seek(SeekFrom::Start(0)).unwrap();
            let mut text = String::new();
            stderr.read_to_string(&mut text).unwrap();
            panic!("radio did not stream; exit {exited:?}: {text}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(radio.discoveries.load(Ordering::Relaxed) > 0);
}

#[test]
fn a_single_radio_streams_without_an_address_from_flags_and_config() {
    for flags in [false, true] {
        assert_streams(vec![MAC_A], flags, None);
    }
}

#[test]
fn two_radios_require_a_choice_and_mac_pinning_streams_from_flags_and_config() {
    for flags in [false, true] {
        let radio = FakeDiscoveryRadio::new(vec![MAC_A, MAC_B]);
        let (_file, mut command) = configured_command(radio.port, flags, None, 0);
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        let error = String::from_utf8_lossy(&out.stderr);
        for needle in [
            "multiple openHPSDR radios answered discovery (2)",
            "02:11:22:33:44:55",
            "02:BB:BB:BB:BB:02",
            "Hermes-Lite",
            "firmware 43",
            "--hpsdr-mac",
            "input.mac",
        ] {
            assert!(error.contains(needle), "{error}");
        }
        // Each row starts with the response's source IP, independent of interface.
        for line in error
            .lines()
            .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        {
            assert!(line
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .is_ok());
        }
        drop(radio);
        assert_streams(vec![MAC_A, MAC_B], flags, Some("02:bb:bb:bb:bb:02"));
    }
}

#[test]
fn no_radio_is_an_actionable_error_within_five_seconds() {
    let radio = FakeDiscoveryRadio::new(vec![]);
    for flags in [false, true] {
        let (_file, mut command) = configured_command(radio.port, flags, None, 0);
        let start = Instant::now();
        let out = command.output().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("no openHPSDR radio answered discovery; run `manta devices`, or set input.host / --hpsdr-host"));
    }
}
