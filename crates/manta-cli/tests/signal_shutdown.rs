//! MAN-85: SIGTERM (and SIGHUP) must enter the same graceful-shutdown drain
//! path SIGINT already uses, and the process must exit 0 rather than with a
//! signal-derived code.
//!
//! Since MAN-78, SIGTERM and SIGHUP go through `signal-hook` on Unix; since
//! MAN-232, SIGINT does too (ctrlc handles only Windows console events), and
//! these tests pin that: drop the SIGTERM registration and
//! `sigterm_exits_zero_through_the_drain_path` goes red. SIGHUP drains
//! only without a `[server]` table; `config_reload.rs` pins the daemon-mode
//! reload. MAN-232 also pins the output hazards: a closed stdout stops a
//! command without a panic (exit 141, `run` through the drain), and a second
//! stop signal ends a run blocked on output at once.
#![cfg(unix)]

mod common;

use common::{fixture_wav, wait_bounded};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The LAST line `manta listen` prints at startup, and therefore the point
/// at which every startup step -- the signal handlers included -- has
/// certainly completed. The tests below wait for it so they can attribute a
/// nonzero exit to a missing drain rather than to a signal that merely
/// arrived before the handler existed.
///
/// As of MAN-122 review round 6 the handler is installed considerably
/// earlier than this line (before `start_spot_server` and its `listening:`
/// banner), so waiting here is now conservative rather than necessary --
/// deliberately so: it keeps these tests measuring the drain and nothing
/// else. `a_signal_at_the_startup_banner_still_exits_through_the_drain`
/// below is the test that pins the earlier boundary.
const READY_MARKER: &str = "manta: listening;";

/// Generous upper bound on signal -> exit. The real number here is ~20 ms
/// (no clients connected, so `tasks::await_all` returns immediately); this
/// only has to be far below the fixture's natural EOF.
const EXIT_BUDGET: Duration = Duration::from_secs(15);

fn spawn_listen() -> Child {
    Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("listen")
        .arg("--source")
        .arg(fixture_wav())
        .arg("--json")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Blocks until the child has registered its signal handler. The stderr
/// reader is dropped on return: later stderr writes then fail with EPIPE,
/// which manta must ignore rather than panic on (MAN-232).
fn await_ready(child: &mut Child) {
    let stderr = child.stderr.take().expect("stderr piped");
    for line in BufReader::new(stderr).lines() {
        if line.unwrap().contains(READY_MARKER) {
            return;
        }
    }
    panic!("manta exited before printing {READY_MARKER:?}");
}

fn assert_signal_drains_and_exits_zero(sig: libc::c_int, name: &str) {
    // `control` is the same command, never signalled. It is what makes the
    // assertion below mean something: without it, a fixture short enough to
    // hit EOF inside the test window would exit 0 on its own and this test
    // would pass even with SIGTERM completely unhandled.
    let mut control = spawn_listen();
    let mut child = spawn_listen();
    await_ready(&mut control);
    await_ready(&mut child);

    assert!(
        child.try_wait().unwrap().is_none(),
        "manta exited on its own before {name} was sent"
    );
    let sent = Instant::now();
    assert_eq!(
        unsafe { libc::kill(child.id() as i32, sig) },
        0,
        "kill({name}) failed: {}",
        std::io::Error::last_os_error()
    );
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .unwrap_or_else(|| panic!("{name} did not stop manta within {EXIT_BUDGET:?}"));
    let elapsed = sent.elapsed();

    let control_still_running = control.try_wait().unwrap().is_none();
    let _ = control.kill();
    let _ = control.wait();
    assert!(
        control_still_running,
        "an unsignalled run of the same fixture also finished within {elapsed:?}, so this \
         test cannot attribute the exit to {name} -- raise FIXTURE_SECONDS"
    );

    assert_eq!(
        status.code(),
        Some(0),
        "{name} must run the shutdown drain and exit 0, got {status:?} after {elapsed:?}"
    );
}

#[test]
fn sigterm_exits_zero_through_the_drain_path() {
    assert_signal_drains_and_exits_zero(libc::SIGTERM, "SIGTERM");
}

#[test]
fn sigint_exits_zero_through_the_drain_path() {
    assert_signal_drains_and_exits_zero(libc::SIGINT, "SIGINT");
}

#[test]
fn sighup_exits_zero_through_the_drain_path() {
    // Not in MAN-85's Gherkin. Since MAN-78, SIGHUP still drains and exits
    // *without* a `[server]` table (a foreground run with no clients,
    // history or uplink to protect, where a terminal hangup should end it);
    // with one it reloads instead, which `config_reload.rs` pins.
    assert_signal_drains_and_exits_zero(libc::SIGHUP, "SIGHUP");
}

/// The `STOPSIGNAL SIGINT` workaround this ticket removes must not come
/// back: with SIGTERM handled, retargeting the container stop signal only
/// hides whether the real path works.
#[test]
fn dockerfile_does_not_retarget_the_container_stop_signal() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Dockerfile");
    let text = std::fs::read_to_string(&path).unwrap();
    let directive = text
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("STOPSIGNAL"));
    assert!(
        directive.is_none(),
        "manta handles SIGTERM natively since MAN-85; the image must use Docker's default \
         stop signal, found: {directive:?}"
    );
}

/// A `[server]` config with every port picked by the system, so tests that
/// run concurrently cannot collide. `status_interval_secs = 0` disables the
/// periodic status task, which has no bearing on what these tests measure.
fn daemon_config(dir: &Path) -> PathBuf {
    let cfg_path = dir.join("server.toml");
    // Port 0 everywhere: this file may run concurrently with anything else
    // in the workspace, and a fixed port would make it flaky rather than
    // meaningful.
    std::fs::write(
        &cfg_path,
        r#"
        [server]
        station_callsign = "W3XYZ"
        bind_addr = "127.0.0.1"
        telnet_port = 0
        json_port = 0
        metrics_port = 0
        status_interval_secs = 0
        "#,
    )
    .unwrap();
    cfg_path
}

/// A daemon over the shared fixture: `run` with `--config` (a `[server]`
/// table) and `--json`, so its stdout is the event stream under test.
fn spawn_daemon(cfg: &Path, stdout: Stdio) -> Child {
    Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["run", "--source"])
        .arg(fixture_wav())
        .args(["--dial-freq-hz", "14060000", "--config"])
        .arg(cfg)
        .arg("--json")
        .env("RUST_LOG", "info")
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// MAN-122 review round 6 (P2). The tests above wait for `READY_MARKER`,
/// which is deliberately the LAST line the daemon prints at startup -- so
/// they cannot see the window this test exists for. With `--config`, the
/// daemon first logs a `listening:` banner naming its bound telnet/JSON/
/// metrics addresses, strictly earlier than that marker. The banner is an
/// *advertisement*: it is the earliest instant at which a supervisor or an
/// operator can read a real address and react, and the reaction under test
/// is "stop the daemon immediately". Before this round the signal handlers
/// ran only AFTER `start_spot_server` returned, so a signal landing here
/// retained its default disposition and killed the process outright,
/// skipping the client/status drain -- with `SIGTERM`'s default kill giving
/// exit code 143, not 0. Signalling at the banner rather than at the marker
/// is the whole point of this test; moving the wait to `READY_MARKER` would
/// make it a duplicate of `sigterm_exits_zero_through_the_drain_path`.
#[test]
fn a_signal_at_the_startup_banner_still_exits_through_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = daemon_config(dir.path());

    let spawn = || spawn_daemon(&cfg_path, Stdio::null());

    // Same control rationale as `assert_signal_drains_and_exits_zero`: a
    // fixture that reached EOF inside the test window would exit 0 on its
    // own and pass this test with the handler installed too late.
    let mut control = spawn();
    let mut child = spawn();
    let control_drain = drain_stderr(&mut control);
    // `listening:` (colon) is the banner; `READY_MARKER` is
    // `manta: listening;` (semicolon), so this match cannot accidentally
    // wait for the later marker.
    let child_drain = await_banner(&mut child);

    assert!(
        child.try_wait().unwrap().is_none(),
        "manta exited on its own before it could be signalled at the banner"
    );
    let sent = Instant::now();
    assert_eq!(
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) },
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
    let status = wait_bounded(&mut child, EXIT_BUDGET).unwrap_or_else(|| {
        panic!("SIGTERM at the banner did not stop manta within {EXIT_BUDGET:?}")
    });
    let elapsed = sent.elapsed();
    let _ = child_drain.join();

    let control_still_running = control.try_wait().unwrap().is_none();
    let _ = control.kill();
    let _ = control.wait();
    let _ = control_drain.join();
    assert!(
        control_still_running,
        "an unsignalled run of the same fixture also finished within {elapsed:?}, so this \
         test cannot attribute the exit to SIGTERM -- raise FIXTURE_SECONDS"
    );

    assert_eq!(
        status.code(),
        Some(0),
        "a SIGTERM delivered as soon as the daemon advertised its bound addresses must still \
         run the shutdown drain and exit 0, got {status:?} after {elapsed:?}"
    );
}

/// Blocks until the child logs its `listening:` startup banner, then keeps
/// draining stderr on a background thread.
///
/// The draining is load-bearing, not tidiness: the banner is an EARLY line,
/// so the daemon still has its `READY_MARKER` line and its readiness event
/// to write afterwards. An unread pipe would fill and block the child's
/// next write (MAN-232 kept blocking writes); a closed one no longer panics
/// it either -- but keeping the reader also keeps those lines, so a failure
/// is reported instead of silently swallowed. `await_ready` above can drop
/// its reader safely because it waits for the last startup line, and manta
/// ignores a failed stderr write since MAN-232.
fn await_banner(child: &mut Child) -> std::thread::JoinHandle<()> {
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr piped"));
    let mut line = String::new();
    let mut saw_banner = false;
    while stderr.read_line(&mut line).unwrap() > 0 {
        if line.contains("listening:") {
            saw_banner = true;
            break;
        }
        line.clear();
    }
    assert!(
        saw_banner,
        "manta exited before logging its `listening:` startup banner"
    );
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    })
}

/// Drains a child's piped stderr to EOF on a background thread, for a child
/// whose output this test never inspects. Same unread-pipe reasoning as
/// `await_banner`: draining keeps the child's later stderr writes from
/// blocking on a full pipe.
fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<()> {
    let mut stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    })
}

// --- MAN-232: a closed stdout -----------------------------------------

/// A 10 s v1 IQ fixture (+ sidecar) in `dir`: long enough for doctor's 3 s
/// floor, short to render and decode.
fn short_iq_fixture(dir: &Path) -> PathBuf {
    let out = dir.join("short");
    std::fs::create_dir_all(&out).unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        fs: 48_000.0,
        duration_s: 10.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, &out).unwrap();
    out.join("v1.wav")
}

/// A child's stdio end from a pipe writer: through a `File`, the `Stdio`
/// conversion std offers for it.
fn stdio_of(w: std::io::PipeWriter) -> Stdio {
    use std::os::fd::FromRawFd as _;
    use std::os::fd::IntoRawFd as _;
    // SAFETY: `into_raw_fd` hands over the fd's sole ownership; the `File`
    // (and then `Stdio`) closes it exactly once.
    Stdio::from(unsafe { std::fs::File::from_raw_fd(w.into_raw_fd()) })
}

/// A pipe whose read end is already closed: every write to the other end
/// fails with EPIPE.
fn closed_pipe() -> std::io::PipeWriter {
    let (r, w) = std::io::pipe().unwrap();
    drop(r);
    w
}

/// A pipe whose buffer is already full, so the child's first stdout write
/// blocks -- deterministic, unlike waiting for manta to fill 64 KiB itself.
/// The read end is returned too: it must be held unread for the whole test,
/// or the child's writes would fail with EPIPE instead of blocking.
fn full_pipe() -> (std::io::PipeReader, std::io::PipeWriter) {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    let (r, w) = std::io::pipe().unwrap();
    // Fill the buffer with non-blocking writes until WouldBlock, then
    // restore the original flags: the child must inherit a blocking pipe.
    let flags = unsafe { libc::fcntl(w.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL: {}", std::io::Error::last_os_error());
    unsafe {
        assert_eq!(
            libc::fcntl(w.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        )
    };
    let mut filled = 0usize;
    let chunk = [0u8; 4096];
    let mut writer = &w;
    loop {
        match writer.write(&chunk) {
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("filling the pipe: {e}"),
        }
    }
    unsafe {
        assert_eq!(
            libc::fcntl(w.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK),
            0
        )
    };
    let _ = filled;
    (r, w)
}

/// Runs `manta args…` with stdout on a closed pipe; returns (status, stderr).
fn run_with_closed_stdout(args: &[&OsStr]) -> (ExitStatus, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(args)
        .stdout(stdio_of(closed_pipe()))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (child.wait().unwrap(), stderr)
}

/// An SNTP target nothing answers: a just-released local UDP port, so doctor
/// sends nothing off-host.
fn dead_ntp() -> String {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    drop(sock);
    addr.to_string()
}

fn assert_141(status: &ExitStatus, stderr: &str, what: &str) {
    assert_eq!(
        status.code(),
        Some(141),
        "{what} must exit 141 (128 + SIGPIPE) when stdout's reader is gone, got {status:?};
stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "{what} panicked: {stderr}");
}

/// Ticket scenario 2: `manta decode <file> | true` used to exit 101 through
/// `println!`'s panic on EPIPE.
#[test]
fn decode_exits_141_without_a_panic_when_stdout_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let wav = short_iq_fixture(dir.path());
    for json in [false, true] {
        let mut args: Vec<&OsStr> = vec!["decode".as_ref(), wav.as_os_str()];
        if json {
            args.push("--json".as_ref());
        }
        let (status, stderr) = run_with_closed_stdout(&args);
        assert_141(
            &status,
            &stderr,
            if json { "decode --json" } else { "decode" },
        );
    }
}

/// Ticket scenario 2 for doctor, in both output modes. In text mode the
/// first setup-check line fails and `Sink::written` stops doctor before the
/// receiver/signal stage; in `--json` mode the whole document is the
/// product, and it is the final write that fails.
#[test]
fn doctor_exits_141_without_a_panic_when_stdout_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let wav = short_iq_fixture(dir.path());
    let ntp = dead_ntp();
    for json in [false, true] {
        let mut args: Vec<&OsStr> = vec![
            "doctor".as_ref(),
            "--duration".as_ref(),
            "3".as_ref(),
            "--source".as_ref(),
            wav.as_os_str(),
            "--source-iq".as_ref(),
            "--ntp-server".as_ref(),
            ntp.as_ref(),
        ];
        if json {
            args.push("--json".as_ref());
        }
        let (status, stderr) = run_with_closed_stdout(&args);
        assert_141(
            &status,
            &stderr,
            if json { "doctor --json" } else { "doctor" },
        );
    }
}

/// `oracle`'s summary is one `println!` on stdout; a closed reader must
/// become 141, not 101.
#[test]
fn oracle_exits_141_without_a_panic_when_stdout_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let wav = short_iq_fixture(dir.path());
    let csv = dir.path().join("empty.csv");
    std::fs::write(
        &csv,
        "callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode\n",
    )
    .unwrap();
    let args: Vec<&OsStr> = vec![
        "oracle".as_ref(),
        wav.as_os_str(),
        csv.as_os_str(),
        "--capture-start".as_ref(),
        "2025-11-29T00:00:00Z".as_ref(),
    ];
    let (status, stderr) = run_with_closed_stdout(&args);
    assert_141(&status, &stderr, "oracle");
}

/// A closed stderr never stops or fails a command: decode's full output is
/// unchanged and the exit stays 0.
#[test]
fn decode_with_a_closed_stderr_still_writes_its_full_output_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let wav = short_iq_fixture(dir.path());
    let reference = Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("decode")
        .arg(&wav)
        .output()
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("decode")
        .arg(&wav)
        .stdout(Stdio::piped())
        .stderr(stdio_of(closed_pipe()))
        .spawn()
        .unwrap();
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert_eq!(
        stdout, reference.stdout,
        "stdout must be identical with a closed stderr"
    );
}

/// A daemon whose stderr reader leaves must keep running and still exit 0
/// through the drain on one SIGTERM. Before MAN-232 it died with exit 101
/// on its own, at the `ready: decoding` record, through
/// tracing-subscriber's fallback `eprintln!`.
#[test]
fn a_daemon_whose_stderr_reader_left_still_exits_zero_through_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = daemon_config(dir.path());
    let mut child = spawn_daemon(&cfg_path, Stdio::null());
    // Drops the stderr reader at readiness: from here on, every stderr
    // write the daemon makes fails with EPIPE.
    await_ready(&mut child);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "a daemon whose stderr reader left must keep running"
    );
    unsafe {
        assert_eq!(libc::kill(child.id() as i32, libc::SIGTERM), 0);
    }
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .expect("SIGTERM did not stop the daemon within the budget");
    assert_eq!(status.code(), Some(0), "{status:?}");
}

// --- MAN-232: `run`/`listen` stop through the drain -------------------

/// Ticket scenario 1: `manta listen --source <file> --source-iq --json |
/// head -1` used to exit 101 mid-stream. Now the first failed write sets
/// the same `stop` a signal sets, the drain runs, and the run exits 141
/// with one note naming why it stopped.
#[test]
fn a_reader_that_exits_early_stops_run_through_the_drain_with_141() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--source"])
        .arg(fixture_wav())
        .args(["--source-iq", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr_reader = child.stderr.take().expect("stderr piped");
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        BufReader::new(stderr_reader)
            .read_to_string(&mut text)
            .unwrap();
        text
    });
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with('{'), "first stdout line: {line:?}");
    drop(reader); // `| head -1`: the reader exits here
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .expect("a closed stdout did not stop manta within the budget");
    let stderr = stderr.join().unwrap();
    assert_141(&status, &stderr, "listen --json | head -1");
    assert!(
        stderr.contains("manta: stdout closed; stopping"),
        "the stop note is missing: {stderr}"
    );
}

/// The daemon form of scenario 1: the same closed-reader stop, through the
/// FULL drain -- `finish()` runs only after `shutdown_runtime_after_drain`,
/// so exit 141 proves the drain sequence completed rather than a panic
/// cutting it short.
#[test]
fn a_daemon_whose_stdout_reader_exits_drains_and_exits_141() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = daemon_config(dir.path());
    let mut child = spawn_daemon(&cfg_path, Stdio::piped());
    let stderr_reader = child.stderr.take().expect("stderr piped");
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        BufReader::new(stderr_reader)
            .read_to_string(&mut text)
            .unwrap();
        text
    });
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    drop(reader);
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .expect("a closed stdout did not stop the daemon within the budget");
    let stderr = stderr.join().unwrap();
    assert_141(&status, &stderr, "daemon --json | head -1");
    assert!(
        stderr.contains("manta: stdout closed; stopping"),
        "the stop note is missing: {stderr}"
    );
}

// --- MAN-232: a second stop signal ------------------------------------

/// The state ticket scenario 3 starts from: a run whose stdout is a pipe
/// that is already full (deterministic -- the child's first stdout write
/// blocks), one stop signal sent, and the process still alive a second
/// later, because `SA_RESTART` restarts the blocked write. The read end is
/// returned and must be held unread until the test is done with the child:
/// dropping it would EPIPE the child into a stop instead.
fn blocked_run_after_one_signal(
    daemon: bool,
    sig: libc::c_int,
    name: &str,
) -> (Child, std::io::PipeReader) {
    let dir = tempfile::tempdir().unwrap();
    let (read_end, write_end) = full_pipe();
    let mut child = if daemon {
        spawn_daemon(&daemon_config(dir.path()), stdio_of(write_end))
    } else {
        Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--source"])
            .arg(fixture_wav())
            .arg("--json")
            .stdout(stdio_of(write_end))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    await_ready(&mut child);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "manta exited before {name} -- it never blocked on the full stdout pipe"
    );
    unsafe { assert_eq!(libc::kill(child.id() as i32, sig), 0) };
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        child.try_wait().unwrap().is_none(),
        "manta ended on the FIRST {name}: either it was not blocked on stdout, so this test \
         proves nothing, or the forced exit fired on the first signal"
    );
    (child, read_end)
}

/// Ticket scenario 3, the Gherkin's daemon: two SIGTERMs one second apart,
/// and the process is gone within 2 s of the second (exit 143, not SIGKILL).
#[test]
fn a_second_sigterm_ends_a_daemon_blocked_on_stdout() {
    let (mut child, read_end) = blocked_run_after_one_signal(true, libc::SIGTERM, "SIGTERM");
    let _held_unread = read_end;
    unsafe { assert_eq!(libc::kill(child.id() as i32, libc::SIGTERM), 0) };
    match wait_bounded(&mut child, Duration::from_secs(2)) {
        Some(status) => assert_eq!(
            status.code(),
            Some(143),
            "a second SIGTERM must end the process at once with 143"
        ),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a second SIGTERM did not end a run blocked on stdout within 2 s");
        }
    }
}

/// The same, for a plain run and SIGINT (exit 130).
#[test]
fn a_second_sigint_ends_a_run_blocked_on_stdout() {
    let (mut child, read_end) = blocked_run_after_one_signal(false, libc::SIGINT, "SIGINT");
    let _held_unread = read_end;
    unsafe { assert_eq!(libc::kill(child.id() as i32, libc::SIGINT), 0) };
    match wait_bounded(&mut child, Duration::from_secs(2)) {
        Some(status) => assert_eq!(
            status.code(),
            Some(130),
            "a second SIGINT must end the process at once with 130"
        ),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a second SIGINT did not end a run blocked on stdout within 2 s");
        }
    }
}

/// One signal to a blocked run still exits 0 through the drain once the
/// reader resumes: the forced exit must not fire on the first signal, and
/// blocking writes stay (the ticket's non-goal).
#[test]
fn one_signal_to_a_run_blocked_on_stdout_still_drains_once_the_reader_resumes() {
    let (mut child, mut read_end) = blocked_run_after_one_signal(false, libc::SIGTERM, "SIGTERM");
    // The reader resumes: the blocked write completes, the loop observes
    // `stop`, and the drain runs to a clean exit 0.
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = read_end.read_to_end(&mut sink);
    });
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .expect("the drained run did not exit within the budget");
    assert_eq!(status.code(), Some(0), "{status:?}");
}
