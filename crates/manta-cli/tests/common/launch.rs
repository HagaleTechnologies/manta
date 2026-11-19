//! MAN-549: the one `manta` launcher and the one port reservation the
//! integration tests share. A leaf that needs only `std`: a root that wants
//! nothing else from `common` includes it alone with
//! `#[path = "common/launch.rs"] mod launch;`, and so does not link what the
//! rest of `common` pulls in (`manta_input`, `regex`, `tungstenite`).
//! `common/mod.rs` re-exports all three functions for the roots that
//! already say `mod common;`.
// Each test binary compiles this file separately and uses only part of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The binary under test, with every `MANTA_*` variable of the test
/// runner's environment removed.
pub fn manta() -> Command {
    without_manta_env(Command::new(env!("CARGO_BIN_EXE_manta")))
}

/// `cmd` with every `MANTA_*` variable of the test runner's environment
/// removed. MAN-261: `run`, `soak`, `doctor`, `health` and `config check`
/// read `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>` and reject unknown
/// `MANTA_*` names, so a variable in the runner's own environment must
/// never leak into the child. Tests that exercise the env tier set theirs
/// explicitly, after this.
pub fn without_manta_env(mut cmd: Command) -> Command {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// `N` distinct loopback TCP ports nothing listens on right now. All `N`
/// listeners are held until every port is read: releasing each before
/// binding the next lets the OS hand the same port out twice (Windows
/// does, and then the config is rejected as "two servers cannot share a
/// port", MAN-371). MAN-570: the ports are free only until this returns,
/// so they are for targets that must stay unreachable (connect targets
/// the daemon dials), never for a bind the daemon performs -- a daemon
/// gets `*_port = 0` and reports what it bound via
/// [`spawn_daemon_reporting_ports`].
/// `doctor_checks.rs` keeps its own band allocator (MAN-333, MAN-410).
pub fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}

/// The loopback listener ports a daemon named in its `listening:` banner
/// after binding `telnet_port = 0`, `json_port = 0` and `metrics_port = 0`
/// (MAN-570). The kernel chose them and the daemon has held them since
/// they existed, so no other process can claim one in between -- the
/// `free_ports` handoff gap cannot happen.
pub struct ReportedPorts {
    pub telnet: u16,
    pub json: u16,
    pub metrics: u16,
}

/// Every stderr line a spawned daemon has written, with its arrival time
/// (seconds since spawn), plus an EOF flag. A condvar wakes a waiter per
/// line, so a test can wait for a line without consuming the ones after
/// it.
struct StderrLog {
    start: Instant,
    state: Mutex<StderrState>,
    changed: Condvar,
}

struct StderrState {
    lines: Vec<(Instant, String)>,
    eof: bool,
}

/// Most stderr lines a failed wait prints, so a chatty daemon cannot bury
/// the panic message (as `common::expect_line_all`).
const DUMP_LINES: usize = 200;

impl StderrLog {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            state: Mutex::new(StderrState {
                lines: Vec::new(),
                eof: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn record(&self, line: String) {
        self.state
            .lock()
            .unwrap()
            .lines
            .push((Instant::now(), line));
        self.changed.notify_all();
    }

    fn ended(&self) {
        self.state.lock().unwrap().eof = true;
        self.changed.notify_all();
    }

    fn dump(&self) -> String {
        let state = self.state.lock().unwrap();
        let skipped = state.lines.len().saturating_sub(DUMP_LINES);
        let mut out = format!("scanned {} line(s)", state.lines.len());
        if skipped > 0 {
            out.push_str(&format!(", the first {skipped} omitted"));
        }
        out.push(':');
        for (at, line) in state.lines[skipped..]
            .iter()
            .map(|(at, l)| (*at, l.as_str()))
        {
            let at = at.duration_since(self.start).as_secs_f64();
            out.push_str(&format!("\n  +{at:.3}s {line}"));
        }
        out
    }

    /// Waits up to `timeout` for a line containing every one of `needles`
    /// and returns it. `child`, when given, names the exit status in the
    /// panic if the stream ended first. Panics naming what was missing and
    /// every line seen (MAN-411: a timeout without the log is
    /// undiagnosable).
    fn wait_line(
        &self,
        needles: &[&str],
        timeout: Duration,
        mut child: Option<&mut Child>,
    ) -> String {
        let matches = |line: &str| needles.iter().all(|n| line.contains(n));
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some((_, line)) = state.lines.iter().find(|(_, l)| matches(l)) {
                return line.clone();
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                panic!(
                    "no stderr line containing {needles:?} within {timeout:?}; {}",
                    self.dump()
                )
            };
            let (guard, _) = self.changed.wait_timeout(state, left).unwrap();
            state = guard;
            if state.eof && !state.lines.iter().any(|(_, l)| matches(l)) {
                let status = child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten())
                    .map(|s| format!(" (exit status {s})"))
                    .unwrap_or_default();
                panic!(
                    "stderr ended{status} before a line containing {needles:?}; {}",
                    self.dump()
                );
            }
        }
    }
}

/// A daemon started by [`spawn_daemon_reporting_ports`]: the child (killed
/// and reaped on drop, so a failed assertion never leaves it running), the
/// ports its banner reported, and its stderr -- every line kept, so a
/// failing assertion can still name what the daemon said (the MAN-234
/// incident was diagnosed from exactly those lines).
pub struct DaemonHandle {
    child: Child,
    /// The ports the daemon bound and reported. Scrape, connect and assert
    /// against these, never against a pre-picked number.
    pub ports: ReportedPorts,
    log: Arc<StderrLog>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl DaemonHandle {
    /// The child, for `try_wait` and signal tests; cleanup stays with the
    /// handle.
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Every stderr line so far, ANSI-stripped and joined by newlines.
    pub fn stderr_so_far(&self) -> String {
        self.log
            .state
            .lock()
            .unwrap()
            .lines
            .iter()
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every stderr line so far with the time it arrived, for ordering
    /// assertions against other events.
    pub fn stderr_lines_so_far(&self) -> Vec<(Instant, String)> {
        self.log.state.lock().unwrap().lines.clone()
    }

    /// Waits up to `timeout` for a stderr line containing every one of
    /// `needles` (as `common::expect_line`), and returns it. Lines are
    /// kept, not consumed: a later wait can still find an earlier line.
    pub fn next_line_containing(&self, needles: &[&str], timeout: Duration) -> String {
        self.log.wait_line(needles, timeout, None)
    }
}

/// `tracing` writes ANSI colour codes even into a pipe; the banner's
/// `key=host:port` tokens only parse once they are stripped (the std-only
/// twin of `common::strip_ansi`, which this leaf must not link `regex`
/// for).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            // Skip the CSI sequence's parameter bytes and its terminator.
            for c in chars.by_ref() {
                if !matches!(c, '0'..='9' | ';') {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses the `telnet=`, `json=` and `metrics=` addresses out of a
/// `listening:` banner line (`manta_server::status::format_startup_banner`
/// prints the real `local_addr()` of every listener, in both the
/// one-source and the `inputs=` multi-input shapes).
fn banner_ports(banner: &str) -> ReportedPorts {
    let port_of = |key: &str| {
        banner
            .split_whitespace()
            .find(|token| token.starts_with(key))
            .and_then(|token| token.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or_else(|| panic!("no {key}<addr> in banner: {banner:?}"))
    };
    ReportedPorts {
        telnet: port_of("telnet="),
        json: port_of("json="),
        metrics: port_of("metrics="),
    }
}

/// Spawns the daemon `cmd` (argv, env and stdout already configured) with
/// stderr forced piped, waits up to `banner_timeout` for its `listening:`
/// banner, and returns the child together with the ports the banner
/// reported (MAN-570). The stderr pipe is drained to EOF on a thread, so
/// a full pipe can never block the daemon (the MAN-232 lesson) and every
/// line survives for later assertions. Panics with the collected lines
/// when the daemon exits or the timeout passes before the banner: a bind
/// failure produces no banner, and its `Address already in use` context
/// is exactly what the panic must show.
pub fn spawn_daemon_reporting_ports(mut cmd: Command, banner_timeout: Duration) -> DaemonHandle {
    // Overwrites whatever the caller set: the banner is read from stderr.
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn manta");
    let stderr = child.stderr.take().expect("stderr was piped");
    let log = Arc::new(StderrLog::new());
    {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            // `read_until`, not `lines()`: a non-UTF-8 byte must not end
            // the drain early (as `common::line_channel`).
            while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                log.record(strip_ansi(String::from_utf8_lossy(&buf).trim_end()));
                buf.clear();
            }
            log.ended();
        });
    }
    let banner = log.wait_line(&["listening:"], banner_timeout, Some(&mut child));
    let ports = banner_ports(&banner);
    DaemonHandle { child, ports, log }
}
