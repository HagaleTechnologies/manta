//! MAN-85: SIGTERM (and SIGHUP) must enter the same graceful-shutdown drain
//! path SIGINT already uses, and the process must exit 0 rather than with a
//! signal-derived code.
//!
//! Nothing else in this repo exercises real OS signal delivery, and no
//! `#[cfg]` in this crate can observe a *dependency's* feature flag, so this
//! file is the only thing standing between `ctrlc`'s `termination` feature
//! and a silent regression: drop the feature from the workspace `Cargo.toml`
//! and `sigterm_exits_zero_through_the_drain_path` goes red.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Printed by `manta listen` immediately after `ctrlc::set_handler` returns.
/// Signalling before that line appears would race the handler registration
/// and kill the child under the OS default disposition no matter what this
/// ticket changed.
const READY_MARKER: &str = "manta: listening;";

/// Replay seconds in the fixture. Only has to outlast the test window --
/// `control_still_running` below turns "too short for this machine" into an
/// explicit failure rather than a false pass.
const FIXTURE_SECONDS: f64 = 300.0;

/// Generous upper bound on signal -> exit. The real number here is ~20 ms
/// (no clients connected, so `tasks::await_all` returns immediately); this
/// only has to be far below the fixture's natural EOF.
const EXIT_BUDGET: Duration = Duration::from_secs(15);

/// One 300 s fixture for the whole file: ~115 MB and a full render, not
/// worth paying three times. Written under `CARGO_TARGET_TMPDIR` (cleaned
/// by `cargo clean`) rather than a `tempfile::TempDir`: Rust never runs
/// destructors for statics, so a shared `TempDir` here would leak 115 MB
/// into the system temp directory on every run. Rendered to a
/// process-unique name and atomically renamed into place, so two
/// concurrent `cargo test` invocations cannot observe a half-written WAV.
fn fixture_wav() -> &'static Path {
    static WAV: OnceLock<PathBuf> = OnceLock::new();
    WAV.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("man85-signal-fixture");
        let wav = root.join("v1.wav");
        if wav.exists() {
            return wav;
        }
        let staging = root.with_extension(format!("staging.{}", std::process::id()));
        std::fs::create_dir_all(&staging).unwrap();
        let spec = manta_testkit::vectors::VectorSpec {
            fs: 48_000.0,
            duration_s: FIXTURE_SECONDS,
            ..manta_testkit::vectors::v1()
        };
        manta_testkit::vectors::write_fixture_set(&spec, &staging).unwrap();
        // Loser of a rename race: another process already published one.
        if std::fs::rename(&staging, &root).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        assert!(
            wav.exists(),
            "fixture missing after publish: {}",
            wav.display()
        );
        wav
    })
    .as_path()
}

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

/// Blocks until the child has registered its signal handler.
fn await_ready(child: &mut Child) {
    let stderr = child.stderr.take().expect("stderr piped");
    for line in BufReader::new(stderr).lines() {
        if line.unwrap().contains(READY_MARKER) {
            return;
        }
    }
    panic!("manta exited before printing {READY_MARKER:?}");
}

fn wait_bounded(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if start.elapsed() > budget {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
    // Not in MAN-85's Gherkin, but `ctrlc`'s `termination` feature is
    // all-or-nothing: it registers SIGINT, SIGTERM *and* SIGHUP behind one
    // signal-agnostic `FnMut()` closure. Pinning SIGHUP here makes that a
    // deliberate, documented behaviour rather than an accident, and forces
    // any future SIGHUP-triggered config reload (broad-review R-08 /
    // MAN-30) to confront the fact that SIGHUP already means "shut down".
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
