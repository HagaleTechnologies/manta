//! ROADMAP M3 acceptance gate, automated: "a stock DX cluster client
//! connects, logs in, and receives well-formed RBN-format spots" -- driven
//! entirely from `manta gen`-shaped output, with no SDR and no hand-built
//! recording (MAN-121).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn manta() -> Command {
    Command::new(env!("CARGO_BIN_EXE_manta"))
}

/// Ephemeral ports HELD by their own listeners until the child is about to
/// start. There is no way to learn a port the CLI assigned itself --
/// `start_spot_server` binds from the TOML, not from an OS-assigned `:0`
/// the caller could read back -- so the ports must be chosen before the
/// child starts.
///
/// Reserving all of them at once and releasing them together is what makes
/// the three DISTINCT (round-14 review): binding and immediately releasing
/// one port at a time let the OS hand the very same port back for the next
/// call, and `cargo test --workspace` runs these two tests in parallel with
/// each other and with the rest of the suite. A duplicate port makes one of
/// `start_spot_server`'s three required binds fail, which the connect
/// retry loop below cannot repair -- it just spends the whole 40 s deadline
/// and reports a timeout, i.e. a flake in a required acceptance test.
///
/// The residual bind-then-release window (release -> child bind) is
/// unavoidable without a `:0`-and-report-back mode in the CLI, but it is
/// now a few milliseconds wide and, when it does lose, fails loudly via
/// `connect_with_retry`'s child-exit check rather than silently.
struct ReservedPorts(Vec<TcpListener>);

impl ReservedPorts {
    fn reserve(n: usize) -> Self {
        ReservedPorts(
            (0..n)
                .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
                .collect(),
        )
    }

    fn ports(&self) -> Vec<u16> {
        self.0
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect()
    }

    /// Hand the ports back to the OS, immediately before spawning the child
    /// that binds them.
    fn release(self) {
        drop(self.0);
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_listen(wav: &std::path::Path, server_toml: &std::path::Path) -> KillOnDrop {
    let child = manta()
        .args(["listen", "--source"])
        .arg(wav)
        .args(["--server-config"])
        .arg(server_toml)
        .args(["--realtime"])
        // Deliberately NO --dial-freq-hz -- the fixture's own <stem>.json
        // sidecar supplies the real RF frequency (MAN-121 Decision 2).
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn manta listen");
    KillOnDrop(child)
}

fn connect_with_retry(child: &mut Child, port: u16, deadline: Instant) -> TcpStream {
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return s,
            Err(e) => {
                // A bind failure in the child (a port lost between release
                // and spawn, a bad config) is terminal: retrying cannot
                // repair it, and without this check the test burns its
                // whole deadline before reporting a bare connect timeout
                // that says nothing about the real cause (round-14
                // review).
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "manta exited with {status} before 127.0.0.1:{port} accepted a \
                         connection (last connect error: {e}) -- it most likely failed to bind"
                    );
                }
                if Instant::now() >= deadline {
                    panic!("could not connect to 127.0.0.1:{port} within deadline: {e}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Reads lines until one contains `needle`, bounded by `deadline`. Never an
/// upper bound the CALLER should tune tightly -- pacing is wall-clock, so
/// this deadline exists only to fail the test cleanly instead of hanging
/// forever, not to assert a specific latency.
fn wait_for_line_containing(
    reader: &mut BufReader<TcpStream>,
    needle: &str,
    deadline: Instant,
) -> String {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for a line containing {needle:?}");
        }
        reader.get_ref().set_read_timeout(Some(remaining)).unwrap();
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("connection closed before a line containing {needle:?} arrived"),
            Ok(_) => {
                if line.contains(needle) {
                    return line;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                panic!("timed out waiting for a line containing {needle:?}");
            }
            Err(e) => panic!("read error waiting for {needle:?}: {e}"),
        }
    }
}

fn write_server_toml(
    dir: &std::path::Path,
    telnet_port: u16,
    json_port: u16,
    metrics_port: u16,
) -> std::path::PathBuf {
    let path = dir.join("server.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\n\
             station_callsign = \"W5AU\"\n\
             bind_addr = \"127.0.0.1\"\n\
             telnet_port = {telnet_port}\n\
             json_port = {json_port}\n\
             metrics_port = {metrics_port}\n"
        ),
    )
    .unwrap();
    path
}

// 30s: the first spot reaches the wire ~12s into paced replay (2s
// calibration + SPEC 2.1's ~2.05s warmup/confirm floor + the vector's own
// lead-in), leaving comfortable margin for a CI runner without making the
// test needlessly slow.
const FIXTURE_DURATION_S: f64 = 30.0;
const SPOT_WAIT_DEADLINE_S: u64 = 40;

#[test]
fn a_stock_telnet_client_receives_a_well_formed_rbn_spot_from_gen_output() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: FIXTURE_DURATION_S,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join("v1.wav");

    let reserved = ReservedPorts::reserve(3);
    let ports = reserved.ports();
    let (telnet_port, json_port, metrics_port) = (ports[0], ports[1], ports[2]);
    let server_toml = write_server_toml(dir.path(), telnet_port, json_port, metrics_port);
    reserved.release();

    let mut child = spawn_listen(&wav, &server_toml);

    let deadline = Instant::now() + Duration::from_secs(SPOT_WAIT_DEADLINE_S);
    let stream = connect_with_retry(&mut child.0, telnet_port, deadline);
    let mut reader = BufReader::new(stream);

    wait_for_line_containing(&mut reader, "login", deadline);
    reader.get_ref().write_all(b"W5AU\r\n").unwrap();

    let spot_line = wait_for_line_containing(&mut reader, "DX de", deadline);

    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the server process must still be running when the spot arrives"
    );

    assert!(spot_line.contains("W1AW"), "spot line: {spot_line}");
    // The vector's own expected_freq_hz (from the sidecar-declared RF
    // frequency, not an audio-tone offset) -- e.g. "14012.4".
    let expected_khz = manifest.expected_freq_hz / 1000.0;
    let khz_str = format!("{expected_khz:.0}");
    assert!(
        spot_line.contains(&khz_str),
        "expected a frequency near {expected_khz:.1} kHz, got: {spot_line}"
    );
}

#[test]
fn a_json_lines_client_receives_the_same_spot() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: FIXTURE_DURATION_S,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join("v1.wav");

    let reserved = ReservedPorts::reserve(3);
    let ports = reserved.ports();
    let (telnet_port, json_port, metrics_port) = (ports[0], ports[1], ports[2]);
    let server_toml = write_server_toml(dir.path(), telnet_port, json_port, metrics_port);
    reserved.release();

    let mut child = spawn_listen(&wav, &server_toml);

    let deadline = Instant::now() + Duration::from_secs(SPOT_WAIT_DEADLINE_S);
    let stream = connect_with_retry(&mut child.0, json_port, deadline);
    stream
        .set_read_timeout(Some(Duration::from_secs(SPOT_WAIT_DEADLINE_S)))
        .unwrap();
    let mut reader = BufReader::new(stream);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for a spot on the JSON Lines port");
        }
        reader.get_ref().set_read_timeout(Some(remaining)).unwrap();
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            panic!("JSON stream closed before a spot arrived");
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // e.g. a keepalive/non-spot control line
        };
        let Some(dx_call) = value.get("dxCall").and_then(|v| v.as_str()) else {
            continue;
        };
        assert_eq!(dx_call, "W1AW", "unexpected dxCall in: {value}");
        let freq = value["frequency"]
            .as_f64()
            .expect("frequency field should be numeric");
        assert!(
            (freq - manifest.expected_freq_hz).abs() < 500.0,
            "expected frequency near {}, got {freq}",
            manifest.expected_freq_hz
        );
        break;
    }
}
