//! MAN-275: the client-side WebSocket frame/message size bound. A hostile
//! or broken KiwiSDR receiver must not be able to make the client buffer
//! an unbounded (tungstenite-default 16 MiB frame / 64 MiB message) amount
//! of data per frame: `KiwiIqSource::connect` configures
//! `MAX_WS_FRAME_BYTES` (64 KiB, ~30x the constant 2068-byte real SND
//! capture), and an over-bound frame must surface as a connection-level
//! `Err` -- which the CLI's `ReconnectingSource` (MAN-73) turns into a
//! logged reconnect, exactly like any other lost connection.
//!
//! MAN-418: the fake server must hold the connection until the client hangs
//! up. A single `ws.read()` returns on the client's first post-handshake
//! `SET`; dropping the socket then, with the rest of `connect()`'s `SET`
//! burst still arriving, resets the connection and fails a later send
//! inside `connect()` (EPIPE on macOS, WSAECONNABORTED on Windows) -- a
//! reset from this side, not the size bound. The assertion also pins the
//! error to tungstenite's `Capacity`, so a reset can never pass for it.

use manta_input::{kiwi::KiwiIqSource, IqSource};
use num_complex::Complex32;
use std::net::TcpListener;
use std::time::Duration;
use tungstenite::error::CapacityError;
use tungstenite::Message;

#[test]
fn frame_larger_than_the_bound_is_rejected_not_buffered() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        ws.read().unwrap(); // SET auth
        ws.send(Message::binary(b"MSG sample_rate=12000.000".to_vec()))
            .unwrap();
        // One SND-tagged binary frame of 64 KiB + 4096 zero bytes: tagged
        // `SND` so that, without a bound, it is decoded as samples rather
        // than falling into the unknown-tag discard.
        let mut oversized = Vec::with_capacity(64 * 1024 + 4096);
        oversized.extend_from_slice(b"SND");
        oversized.resize(64 * 1024 + 4096, 0);
        match ws.send(Message::binary(oversized)) {
            Ok(()) => {}
            // The client rejects the frame from its header and drops the
            // connection with the body still unread, so this write can race
            // that reset (WSAECONNABORTED/WSAECONNRESET on Windows, EPIPE on
            // macOS). A peer-close here is the expected rejection; the
            // client-side assertions below decide.
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) => {}
            Err(e) => panic!("sending the oversized frame: {e}"),
        }
        // Park until the client hangs up (its read errors on the oversized
        // frame, then it drops the socket): drain every SET it sends, so
        // neither a close nor a reset from this side can be what ends the
        // client's connection -- only the frame's size may end it. The 30 s
        // read timeout bounds this if the client never hangs up.
        while ws.read().is_ok() {}
    });

    // Whichever call meets the oversized frame -- today read(), since
    // connect()'s handshake stops at `sample_rate=` -- it must be the
    // connection-level size-bound error.
    let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
    let outcome = KiwiIqSource::connect("127.0.0.1", port, 14_025_000.0, "")
        .and_then(|mut src| src.read(&mut buf));
    let err = match outcome {
        Ok(n) => panic!(
            "a frame larger than MAX_WS_FRAME_BYTES must surface a connection-level Err, \
             not be accepted and decoded -- read returned Ok({n})"
        ),
        Err(e) => e,
    };
    assert!(
        err.chain().any(|c| matches!(
            c.downcast_ref::<tungstenite::Error>(),
            Some(tungstenite::Error::Capacity(
                CapacityError::MessageTooLong { .. }
            ))
        )),
        "the Err must be the size bound (tungstenite Capacity), not some other \
         connection failure: {err:#}"
    );
    server.join().expect("the fake KiwiSDR panicked");
}
