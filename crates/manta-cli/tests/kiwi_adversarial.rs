//! MAN-275, the ticket's first scenario: a malformed or adversarial
//! WebSocket, MSG or SND frame -- or a clean `Close` -- from an
//! operator-chosen public KiwiSDR receiver must not stop the daemon, the
//! same guarantee MAN-22 established for HPSDR.
//!
//! These are regression locks for behavior that already held on main
//! before this ticket: connection-level failures are absorbed by the CLI's
//! generic `ReconnectingSource` (MAN-73), which wraps every live source at
//! the composition root, and parser-level junk (unknown tags, too-short
//! SND bodies) is discarded in place by `kiwi.rs` itself. Each test drives
//! the real `manta` binary against a scripted loopback fake receiver (the
//! same harness `signal_shutdown.rs` uses) and asserts the daemon's
//! observable outcome: reconnect logged and still running, in-place
//! discard with NO reconnect, or a prompt clean startup failure.
//!
//! Run serialized (`-- --test-threads=1`): the SND pacing is real-time,
//! matching the existing fake-Kiwi tests' discipline.

mod common;

use common::{
    accept_kiwi_session, expect_line, line_channel, send_snd_frames, wait_bounded, KillOnDrop,
};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tungstenite::Message;

/// Generous bound for a line that should arrive within seconds: startup
/// calibration alone can outlast a slow machine's first seconds.
const WAIT: Duration = Duration::from_secs(60);
/// How long the fake receiver streams valid frames before misbehaving.
const STREAM_FOR: Duration = Duration::from_secs(2);

/// Runs `manta listen` against a fake KiwiSDR on `kiwi_port`, stderr piped.
fn spawn_manta(kiwi_port: u16) -> KillOnDrop {
    KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "listen",
                "--kiwi-host",
                "127.0.0.1",
                "--kiwi-port",
                &kiwi_port.to_string(),
                "--kiwi-freq-hz",
                "14025000",
            ])
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn manta"),
    )
}

/// Collects stderr lines until `idle` passes with no new line (bounded
/// overall by `WAIT`), then returns everything seen. For assertions that
/// a line NEVER appeared: absence after an idle window is the proof.
fn drain_until_idle(rx: &mpsc::Receiver<String>, idle: Duration) -> Vec<String> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(idle.min(left)) {
            Ok(line) => seen.push(line),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return seen
            }
        }
    }
}

/// Accepts manta's connection and plays the WebSocket handshake WITHOUT
/// ever sending `MSG sample_rate=` -- unlike `common::accept_kiwi_session`,
/// which completes `KiwiIqSource::connect`'s handshake. For the
/// Close-during-the-initial-handshake scenario: `connect()`'s own read
/// loop is the code under test, so it must still be running when the
/// `Close` lands.
fn accept_kiwi_session_without_sample_rate(
    listener: &TcpListener,
    done: &AtomicBool,
) -> Result<tungstenite::WebSocket<TcpStream>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + WAIT;
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
    Ok(ws)
}

/// A clean WebSocket `Close` mid-stream is the ticket's headline case
/// ("even a clean Close frame"): the daemon must log the loss, reconnect
/// and keep running -- not exit.
#[test]
fn clean_close_midstream_is_survived() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicUsize::new(0));
    let (second_accept_tx, second_accept_rx) = mpsc::channel();
    let fake = std::thread::spawn({
        let done = done.clone();
        let accepted = accepted.clone();
        move || -> Result<(), String> {
            let mut ws = accept_kiwi_session(&listener, &done)?;
            accepted.fetch_add(1, Ordering::SeqCst);
            let until = Instant::now() + STREAM_FOR;
            send_snd_frames(&mut ws, || {
                done.load(Ordering::Relaxed) || Instant::now() >= until
            });
            // The adversarial event: a clean Close, mid-stream.
            let _ = ws.send(Message::Close(None));
            // Hold the connection until the client hangs up. Dropping it
            // now, with the SET burst and keepalives still unread, would
            // send an RST that can replace the Close (MAN-421, as MAN-418).
            while ws.read().is_ok() {}
            // The daemon should reconnect (1 s backoff): accept again and
            // stream once more so the reconnected line has data to chew.
            let mut ws2 = accept_kiwi_session(&listener, &done)?;
            accepted.fetch_add(1, Ordering::SeqCst);
            let _ = second_accept_tx.send(());
            send_snd_frames(&mut ws2, || done.load(Ordering::Relaxed));
            Ok(())
        }
    });

    let mut child = spawn_manta(kiwi_port);
    let stderr = line_channel(child.0.stderr.take().expect("stderr piped"));

    expect_line(
        &stderr,
        &["source kiwi lost", "closed the connection"],
        WAIT,
    );
    expect_line(&stderr, &["source kiwi reconnected"], WAIT);
    second_accept_rx
        .recv_timeout(WAIT)
        .expect("the fake receiver saw the second connection");
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the daemon must still be running after a clean Close"
    );

    done.store(true, Ordering::Relaxed);
    fake.join().expect("the fake KiwiSDR panicked").unwrap();
}

/// A raw WebSocket protocol violation mid-stream -- bytes not constructible
/// through tungstenite's own API, written straight to the TCP stream -- must
/// be survived the same way a clean Close is.
#[test]
fn raw_protocol_violation_midstream_is_survived() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || -> Result<(), String> {
            let mut ws = accept_kiwi_session(&listener, &done)?;
            let until = Instant::now() + STREAM_FOR;
            send_snd_frames(&mut ws, || {
                done.load(Ordering::Relaxed) || Instant::now() >= until
            });
            // The adversarial event: 64 raw 0xFF bytes directly onto the
            // socket. 0xF is a reserved WebSocket opcode, so this is a
            // genuine protocol violation, not merely a frame kiwi.rs's own
            // tag dispatch would ignore.
            ws.get_mut()
                .write_all(&[0xFFu8; 64])
                .map_err(|e| format!("writing raw violation bytes: {e}"))?;
            // Hold the poisoned connection until the client hangs up on
            // it (its read errors first), so the violation -- not a
            // reset from our side -- is what ends the client's read.
            while ws.read().is_ok() {}
            let mut ws2 = accept_kiwi_session(&listener, &done)?;
            send_snd_frames(&mut ws2, || done.load(Ordering::Relaxed));
            Ok(())
        }
    });

    let mut child = spawn_manta(kiwi_port);
    let stderr = line_channel(child.0.stderr.take().expect("stderr piped"));

    // tungstenite rejects the reserved opcode 0xF in the frame header,
    // before it looks at the bogus length.
    expect_line(
        &stderr,
        &[
            "source kiwi lost",
            "WebSocket protocol error",
            "invalid opcode: 15",
        ],
        WAIT,
    );
    expect_line(&stderr, &["source kiwi reconnected"], WAIT);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the daemon must still be running after a raw protocol violation"
    );

    done.store(true, Ordering::Relaxed);
    fake.join().expect("the fake KiwiSDR panicked").unwrap();
}

/// Parser-level junk is discarded in place, with NO reconnect: a
/// well-formed binary frame with an unknown 3-byte tag, and an SND-tagged
/// frame whose body is shorter than the 17-byte header, interleaved with
/// valid SND frames. The daemon keeps running and never logs a loss.
#[test]
fn unknown_tag_and_truncated_frames_are_discarded_in_place() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || -> Result<(), String> {
            let mut ws = accept_kiwi_session(&listener, &done)?;
            // Same frame layout and real-time pacing as
            // common::send_snd_frames, with adversarial-but-legal frames
            // interleaved every 8th frame.
            const PAIRS: usize = 512;
            let frame_period = Duration::from_secs_f64(PAIRS as f64 / 12_000.0);
            let mut rng: u32 = 0x1234_5678;
            let mut seq: u32 = 0;
            // Streams until `done` (set only after the assertions below):
            // the connection must stay up for the whole assertion window,
            // so a hang-up -- which legitimately logs a lost line -- can
            // never be mistaken for (or mask) the in-place discard under
            // test. If the test dies first, the daemon is killed and the
            // next send fails, ending this loop too.
            let start = Instant::now();
            while !done.load(Ordering::Relaxed) {
                let mut frame = Vec::with_capacity(3 + 17 + PAIRS * 4);
                frame.extend_from_slice(b"SND");
                frame.push(0x08);
                frame.extend_from_slice(&seq.to_le_bytes());
                frame.extend_from_slice(&[0u8; 2 + 10]);
                for _ in 0..PAIRS * 2 {
                    // A deterministic LCG: low-level noise, never digital
                    // silence.
                    rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let sample = ((rng >> 16) as i16) / 64;
                    frame.extend_from_slice(&sample.to_be_bytes());
                }
                if ws.send(Message::binary(frame)).is_err() {
                    break;
                }
                seq = seq.wrapping_add(1);
                if seq.is_multiple_of(8) {
                    let chaff = if (seq / 8).is_multiple_of(2) {
                        // Well-formed binary frame, unrecognized tag.
                        Message::binary([b"ZZZ".as_slice(), &[0u8; 32]].concat())
                    } else {
                        // SND-tagged frame with a body shorter than the
                        // 17-byte header (flags+seq+smeter+gps).
                        Message::binary(b"SND\x08\x01\x02".to_vec())
                    };
                    if ws.send(chaff).is_err() {
                        break;
                    }
                }
                let due = start + frame_period * seq;
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
            Ok(())
        }
    });

    let mut child = spawn_manta(kiwi_port);
    let stderr = line_channel(child.0.stderr.take().expect("stderr piped"));

    // Let ~5 s of mixed valid/adversarial frames flow, then drain with a
    // bounded idle wait and assert the absence: still running, no loss.
    std::thread::sleep(Duration::from_secs(7));
    let seen = drain_until_idle(&stderr, Duration::from_secs(2));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the daemon must still be running through unknown-tag/truncated frames"
    );
    assert!(
        seen.iter()
            .all(|l| !l.contains("source kiwi lost") && !l.contains("reconnecting in")),
        "unknown-tag and truncated frames must be discarded in place, with no \
         reconnect; stderr:\n{}",
        seen.join("\n")
    );

    done.store(true, Ordering::Relaxed);
    fake.join().expect("the fake KiwiSDR panicked").unwrap();
}

/// One WebSocket frame larger than `kiwi.rs`'s `MAX_WS_FRAME_BYTES`
/// (MAN-275) mid-stream: the client's read errors (tungstenite Capacity),
/// the connection is dropped and reconnected -- the daemon keeps running.
/// A hostile receiver gets its connection reset, never an unbounded
/// per-frame buffer.
#[test]
fn oversized_frame_midstream_is_survived() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || -> Result<(), String> {
            let mut ws = accept_kiwi_session(&listener, &done)?;
            let until = Instant::now() + STREAM_FOR;
            send_snd_frames(&mut ws, || {
                done.load(Ordering::Relaxed) || Instant::now() >= until
            });
            // The adversarial event: one SND-tagged binary frame of
            // 64 KiB + 4096 zero bytes -- over the client-side bound,
            // but well under tungstenite's 16 MiB frame default, so on an
            // unbounded client it is accepted and decoded, not rejected
            // by the library. Keep streaming valid frames afterwards so
            // ONLY the oversized frame can be what ends the connection.
            let mut oversized = Vec::with_capacity(64 * 1024 + 4096);
            oversized.extend_from_slice(b"SND");
            oversized.resize(64 * 1024 + 4096, 0);
            ws.send(Message::binary(oversized))
                .map_err(|e| format!("sending the oversized frame: {e}"))?;
            send_snd_frames(&mut ws, || done.load(Ordering::Relaxed));
            // The client drops the poisoned connection once its read
            // errors; accept and stream once more.
            let mut ws2 = accept_kiwi_session(&listener, &done)?;
            send_snd_frames(&mut ws2, || done.load(Ordering::Relaxed));
            Ok(())
        }
    });

    let mut child = spawn_manta(kiwi_port);
    let stderr = line_channel(child.0.stderr.take().expect("stderr piped"));

    expect_line(&stderr, &["source kiwi lost"], WAIT);
    expect_line(&stderr, &["source kiwi reconnected"], WAIT);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the daemon must still be running after an oversized frame"
    );

    done.store(true, Ordering::Relaxed);
    fake.join().expect("the fake KiwiSDR panicked").unwrap();
}

/// A `Close` that lands during the initial handshake (before `sample_rate=`
/// ever arrives) is a startup failure: the run must end promptly and
/// cleanly with the handshake error named -- not hang, and not reconnect
/// forever (MAN-73's "Startup fail-fast" design).
#[test]
fn close_during_initial_handshake_fails_fast_not_hangs() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || -> Result<(), String> {
            let mut ws = accept_kiwi_session_without_sample_rate(&listener, &done)?;
            ws.send(Message::Close(None))
                .map_err(|e| format!("sending Close: {e}"))?;
            // Drain manta's side of the closing handshake (or its EOF)
            // so the socket is not reset with the Close still in flight.
            let _ = ws.read();
            Ok(())
        }
    });

    let mut child = spawn_manta(kiwi_port);
    let stderr = line_channel(child.0.stderr.take().expect("stderr piped"));

    let status = wait_bounded(&mut child.0, Duration::from_secs(15)).unwrap_or_else(|| {
        panic!("a Close during the initial handshake must end the run promptly, not hang")
    });
    assert!(
        !status.success(),
        "startup must fail cleanly, got {status:?}"
    );
    // The channel disconnects at stderr's EOF, i.e. once manta has exited.
    let all_lines: Vec<String> = stderr.iter().collect();
    assert!(
        all_lines.iter().any(|l| l.contains("handshake")),
        "the handshake read error must be named in the output; stderr:\n{}",
        all_lines.join("\n")
    );

    done.store(true, Ordering::Relaxed);
    fake.join().expect("the fake KiwiSDR panicked").unwrap();
}
