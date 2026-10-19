//! MAN-275: the client-side WebSocket frame/message size bound. A hostile
//! or broken KiwiSDR receiver must not be able to make the client buffer
//! an unbounded (tungstenite-default 16 MiB frame / 64 MiB message) amount
//! of data per frame: `KiwiIqSource::connect` configures
//! `MAX_WS_FRAME_BYTES` (64 KiB, ~30x the constant 2068-byte real SND
//! capture), and an over-bound frame must surface as a connection-level
//! `Err` from `read()` -- which the CLI's `ReconnectingSource` (MAN-73)
//! turns into a logged reconnect, exactly like any other lost connection.

use manta_input::{kiwi::KiwiIqSource, IqSource};
use num_complex::Complex32;
use std::net::TcpListener;
use std::time::Duration;
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
        ws.send(Message::binary(oversized)).unwrap();
        // Hold the connection open until the client hangs up (or the 30 s
        // read timeout fires), so neither a close nor a reset from this side
        // can be what ends the client's read -- only the frame's size may
        // end it. A single read was not a hold: it returned on the client's
        // next SET, and the close that followed reset the connection
        // mid-handshake (MAN-411: EPIPE on macOS, 10053 on Windows).
        while ws.read().is_ok() {}
    });

    let mut src = KiwiIqSource::connect("127.0.0.1", port, 14_025_000.0, "").unwrap();
    let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
    let outcome = src.read(&mut buf);
    assert!(
        outcome.is_err(),
        "a frame larger than MAX_WS_FRAME_BYTES must surface a connection-level Err \
         (tungstenite Capacity), not be accepted and decoded -- read returned {:?}",
        outcome.map(|_| "Ok(..)")
    );
    drop(src);
    let _ = server.join();
}
