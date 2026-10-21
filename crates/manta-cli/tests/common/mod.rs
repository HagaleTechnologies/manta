//! Helpers shared by the tests that drive a real `manta` process and its
//! signals (`signal_shutdown.rs`, MAN-85; `config_reload.rs`, MAN-78), and
//! a fake KiwiSDR for live-source daemons (`config_reload.rs`;
//! `list_refresh.rs`, MAN-316; `signal_shutdown.rs`'s reconnect stall,
//! MAN-252).
// Each test binary compiles this module separately and uses only part of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Replay seconds in the fixture. Only has to outlast the test window --
/// every caller turns "too short for this machine" into an explicit
/// failure rather than a false pass.
pub const FIXTURE_SECONDS: f64 = 300.0;

/// One 300 s fixture for every test binary: ~115 MB and a full render, not
/// worth paying more than once. Written under `CARGO_TARGET_TMPDIR` (cleaned
/// by `cargo clean`) rather than a `tempfile::TempDir`: Rust never runs
/// destructors for statics, so a shared `TempDir` here would leak 115 MB
/// into the system temp directory on every run. Rendered to a
/// process-unique name and atomically renamed into place, so two
/// concurrent `cargo test` invocations (or two test binaries) cannot
/// observe a half-written WAV.
pub fn fixture_wav() -> &'static Path {
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

pub fn wait_bounded(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
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

/// `tracing` writes ANSI colour codes even into a pipe; structured fields
/// (`blocklist_calls=1`) only match once they are stripped.
pub fn strip_ansi(line: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap())
        .replace_all(line, "")
        .into_owned()
}

/// Forwards every line `reader` yields, ANSI-stripped, over a channel from
/// a background thread, and keeps draining to EOF so the child never blocks
/// on a write to a full pipe (MAN-232 kept blocking writes; a closed pipe
/// no longer panics it). The channel disconnects at EOF -- the child closed
/// its end, i.e. exited.
pub fn line_channel(reader: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        // `read_until`, not `lines()`: a telnet peer may send non-UTF-8
        // bytes (IAC), which must not end the stream early.
        while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&buf);
            // A dropped receiver just means nobody is listening any more;
            // keep draining.
            let _ = tx.send(strip_ansi(line.trim_end()));
            buf.clear();
        }
    });
    rx
}

/// Waits up to `timeout` for a line containing every one of `needles`, and
/// returns it. Panics naming what was missing; a disconnected channel means
/// the stream ended first (the process exited, or the socket closed).
pub fn expect_line(rx: &Receiver<String>, needles: &[&str], timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                if needles.iter().all(|n| line.contains(n)) {
                    return line;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("no line containing {needles:?} within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!(
                "the stream ended before a line containing {needles:?} -- if the daemon \
                 exited, the fixture ended before it; raise FIXTURE_SECONDS"
            ),
        }
    }
}

/// How long the fake KiwiSDR waits for manta to connect.
const KIWI_ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Kills and reaps the daemon on drop, so a failed assertion never leaves it
/// reconnecting to a fake KiwiSDR that has gone away.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Plays a KiwiSDR just far enough for `KiwiIqSource::connect`
/// (crates/manta-input/src/kiwi.rs) to return, then sends only pings, every
/// 200 ms, until `done` or the client hangs up. No SND frame ever arrives, so
/// the daemon's decode thread waits in the socket `recv`, and each ping
/// resets the source's stall bound.
pub fn fake_kiwi(listener: TcpListener, done: Arc<AtomicBool>) -> Result<(), String> {
    serve_fake_kiwi(listener, done, false)
}

/// MAN-316: [`fake_kiwi`], but streaming real-time `SND` frames of quiet
/// noise instead of pings, so the daemon finishes its startup calibration,
/// logs `ready: decoding` and keeps its decode loop (and `/healthz`) live.
pub fn fake_kiwi_streaming(listener: TcpListener, done: Arc<AtomicBool>) -> Result<(), String> {
    serve_fake_kiwi(listener, done, true)
}

fn serve_fake_kiwi(
    listener: TcpListener,
    done: Arc<AtomicBool>,
    stream_iq: bool,
) -> Result<(), String> {
    use tungstenite::Message;

    let mut ws = accept_kiwi_session(&listener, &done)?;
    if !stream_iq {
        while !done.load(Ordering::Relaxed) {
            if ws.send(Message::Ping(Default::default())).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        return Ok(());
    }
    send_snd_frames(&mut ws, || done.load(Ordering::Relaxed));
    Ok(())
}

/// MAN-252: a loopback listener whose accept queue holds one connection
/// (`listen(0)` via socket2). Once one unaccepted connection sits in it,
/// Linux drops further SYNs, so a client's connect stalls for its whole
/// connect timeout.
pub fn backlog_zero_listener() -> TcpListener {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    socket.bind(&addr.into()).unwrap();
    socket.listen(0).unwrap();
    socket.into()
}

/// MAN-252: plays one KiwiSDR session -- handshake, `sample_rate`, real-time
/// SND noise for `stream_for` -- then parks a connection in the (backlog-0)
/// accept queue and hangs up, so the daemon's reopen stalls in TCP connect
/// for kiwi.rs's CONNECT_TIMEOUT (10 s) on its detached connect thread: a
/// stall SIGTERM's EINTR cannot cut short (unlike a stalled handshake read
/// on the decode thread). Sends on `hung_up` after the hang-up; keeps the
/// parked connection and the listener until `done`.
pub fn fake_kiwi_then_stalled_reconnect(
    listener: TcpListener,
    stream_for: Duration,
    hung_up: mpsc::Sender<()>,
    done: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut ws = accept_kiwi_session(&listener, &done)?;
    let until = Instant::now() + stream_for;
    send_snd_frames(&mut ws, || {
        done.load(Ordering::Relaxed) || Instant::now() >= until
    });
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let parked = TcpStream::connect(addr).map_err(|e| format!("parking a connection: {e}"))?;
    // Unread client data in the receive buffer makes the close a reset,
    // which the daemon reads as a lost connection at once.
    drop(ws);
    let _ = hung_up.send(());
    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(parked);
    Ok(())
}

/// Accepts manta's KiwiSDR connection and plays the handshake just far
/// enough for `KiwiIqSource::connect` (crates/manta-input/src/kiwi.rs) to
/// return. `pub` for `kiwi_adversarial.rs` (MAN-275), which scripts its own
/// misbehaving-receiver scenarios on top of this.
pub fn accept_kiwi_session(
    listener: &TcpListener,
    done: &AtomicBool,
) -> Result<tungstenite::WebSocket<TcpStream>, String> {
    use tungstenite::Message;

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + KIWI_ACCEPT_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return Err("manta never connected to the fake KiwiSDR".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("accept: {e}")),
        }
    };
    // BSD/macOS sockets inherit the listener's non-blocking mode.
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut ws = tungstenite::accept(stream).map_err(|e| format!("WebSocket handshake: {e}"))?;
    ws.read().map_err(|e| format!("reading SET auth: {e}"))?;
    // 12000 Hz / 93.75 Hz per channel = 128 channels: a valid rate.
    ws.send(Message::binary(b"MSG sample_rate=12000.000".to_vec()))
        .map_err(|e| format!("sending sample_rate: {e}"))?;
    Ok(ws)
}

/// Streams real-time `SND` frames of quiet noise until `stop()` or the
/// client hangs up. `pub` for `kiwi_adversarial.rs` (MAN-275).
pub fn send_snd_frames(ws: &mut tungstenite::WebSocket<TcpStream>, stop: impl Fn() -> bool) {
    use tungstenite::Message;

    // KiwiSDR's native frame: 512 complex pairs, i.e. 1/23.4375 s at
    // 12000 Hz. Layout per kiwi.rs's module docs: flags (big-endian
    // samples), little-endian seq, S-meter, 10-byte GPS block, then I,Q.
    const PAIRS: usize = 512;
    let frame_period = Duration::from_secs_f64(PAIRS as f64 / 12_000.0);
    let mut rng: u32 = 0x1234_5678;
    let mut seq: u32 = 0;
    let start = Instant::now();
    while !stop() {
        let mut frame = Vec::with_capacity(3 + 17 + PAIRS * 4);
        frame.extend_from_slice(b"SND");
        frame.push(0x08);
        frame.extend_from_slice(&seq.to_le_bytes());
        frame.extend_from_slice(&[0u8; 2 + 10]);
        for _ in 0..PAIRS * 2 {
            // A deterministic LCG: low-level noise, never digital silence.
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let sample = ((rng >> 16) as i16) / 64;
            frame.extend_from_slice(&sample.to_be_bytes());
        }
        if ws.send(Message::binary(frame)).is_err() {
            break;
        }
        seq = seq.wrapping_add(1);
        // Real-time pacing against the start, so sleep jitter never
        // accumulates into a stall.
        let due = start + frame_period * seq;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
}

/// MAN-310: plays `n` KiwiSDR sessions on one listener, one per stream of a
/// band plan. Sends each session's `SET mod=iq ... freq=...` tune line on
/// `tunes`, then streams SND noise until `done`.
pub fn fake_kiwi_sessions(
    listener: TcpListener,
    n: usize,
    tunes: mpsc::Sender<String>,
    done: Arc<AtomicBool>,
) -> Result<(), String> {
    use tungstenite::Message;

    let mut sessions = Vec::with_capacity(n);
    for _ in 0..n {
        let mut ws = accept_kiwi_session(&listener, &done)?;
        let tunes = tunes.clone();
        let done = done.clone();
        sessions.push(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match ws.read() {
                    Ok(Message::Text(text)) if text.starts_with("SET mod=iq") => {
                        let _ = tunes.send(text.to_string());
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            send_snd_frames(&mut ws, || done.load(Ordering::Relaxed));
        }));
    }
    for s in sessions {
        let _ = s.join();
    }
    Ok(())
}
