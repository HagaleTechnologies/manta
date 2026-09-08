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

/// The three ports one `manta listen --server-config` child binds.
struct Ports {
    telnet: u16,
    json: u16,
    metrics: u16,
}

/// Ask the OS for three free ports, then release them. There is no way to
/// learn a port the CLI assigned itself -- `start_spot_server` binds from
/// the TOML, not from an OS-assigned `:0` the caller could read back -- so
/// the ports must be chosen before the child starts.
///
/// All three listeners are held open SIMULTANEOUSLY and released together
/// (round-3 review): asking three times in a row and dropping each
/// listener before the next request lets the OS hand the same ephemeral
/// port back twice, which makes the child fail to bind its second
/// listener -- deterministically, and for reasons that have nothing to do
/// with what this test asserts. Holding them makes the triple distinct by
/// construction.
///
/// The remaining bind-then-release window against ANOTHER process is not
/// closable from here (the child must do the real bind itself), so
/// `spawn_and_connect` below treats a child that dies during startup as a
/// lost race and retries with a fresh triple, rather than spending the
/// whole connect deadline on a socket that will never exist.
fn free_ports() -> Ports {
    let held: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<u16> = held
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    drop(held);
    Ports {
        telnet: ports[0],
        json: ports[1],
        metrics: ports[2],
    }
}

/// How many times to re-pick ports and respawn when the child dies during
/// startup (i.e. lost the bind race to another process on the machine).
const SPAWN_ATTEMPTS: usize = 3;
/// Per-attempt budget for "the child came up and its listener is
/// reachable". Startup is well under a second; this is only a ceiling for
/// declaring an attempt lost, not a latency assertion.
const STARTUP_DEADLINE_S: u64 = 10;

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

/// Connect to `port`, retrying while the child is still coming up. Returns
/// `Err` -- rather than panicking -- when the child has EXITED (it lost the
/// bind race, so no amount of further retrying can help) or when the
/// deadline passes, so the caller can respawn on a fresh port triple.
fn connect_with_retry(
    port: u16,
    child: &mut Child,
    deadline: Instant,
) -> Result<TcpStream, String> {
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return Ok(s),
            Err(e) => {
                if let Some(status) = child.try_wait().expect("poll child") {
                    return Err(format!(
                        "child exited with {status} before 127.0.0.1:{port} was reachable \
                         (likely lost the port bind race): {e}"
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "could not connect to 127.0.0.1:{port} within deadline: {e}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Spawn the child against a fresh port triple and connect to the port
/// `select` picks out of it, retrying the WHOLE spawn (new ports included)
/// if the child dies during startup. That is the only repair available for
/// a lost bind race -- reconnecting to a port nothing is listening on
/// cannot fix it, and burning the spot deadline on that turns a stolen
/// port into a slow flake instead of a fast retry (round-3 review).
fn spawn_and_connect(
    dir: &std::path::Path,
    wav: &std::path::Path,
    select: fn(&Ports) -> u16,
) -> (KillOnDrop, TcpStream) {
    let mut failures = Vec::new();
    for _ in 0..SPAWN_ATTEMPTS {
        let ports = free_ports();
        let server_toml = write_server_toml(dir, &ports);
        let mut child = spawn_listen(wav, &server_toml);
        let deadline = Instant::now() + Duration::from_secs(STARTUP_DEADLINE_S);
        match connect_with_retry(select(&ports), &mut child.0, deadline) {
            Ok(stream) => return (child, stream),
            // `child` drops here, killing the process before the retry.
            Err(e) => failures.push(e),
        }
    }
    panic!("manta listen never became reachable in {SPAWN_ATTEMPTS} attempts: {failures:?}");
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

fn write_server_toml(dir: &std::path::Path, ports: &Ports) -> std::path::PathBuf {
    let path = dir.join("server.toml");
    let (telnet_port, json_port, metrics_port) = (ports.telnet, ports.json, ports.metrics);
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

    let (mut child, stream) = spawn_and_connect(dir.path(), &wav, |p| p.telnet);

    let deadline = Instant::now() + Duration::from_secs(SPOT_WAIT_DEADLINE_S);
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

    let (_child, stream) = spawn_and_connect(dir.path(), &wav, |p| p.json);

    let deadline = Instant::now() + Duration::from_secs(SPOT_WAIT_DEADLINE_S);
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
