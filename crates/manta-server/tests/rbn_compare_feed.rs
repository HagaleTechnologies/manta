//! MAN-301 Phase 4: the read-only RBN feed client (`rbn_compare::feed`)
//! against a fake feed on `127.0.0.1:0`. manta is the connecting client
//! here, as in `uplink_acceptance.rs`.
//!
//! The login tests pin MAN-90's finding C7: a login prompt with no
//! trailing newline must not stall the login the way `uplink.rs`'s line
//! read does.

use manta_server::metrics::Metrics;
use manta_server::rbn_compare::feed::{
    self, FeedConfig, FeedEvent, FEED_IDLE_TIMEOUT, LOGIN_PROMPT_WAIT,
};
use manta_server::rbn_compare::grade::RbnReport;
use manta_server::rbn_compare::Clock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const LOGIN: &str = "W3XYZ-1";
const CW_LINE: &str =
    "DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z\r\n";
const CW_LINE_2: &str =
    "DX de KM3T-2-#: 7023.40  JA1ABC         CW    18 dB  31 WPM  CQ      0237Z\r\n";
const RTTY_LINE: &str =
    "DX de W3OA-#:   14080.00  K1ABC          RTTY  15 dB  45 BPS  CQ      0237Z\r\n";
const FT8_LINE: &str =
    "DX de W3OA-#:   14074.00  K1ABC          FT8   -5 dB  25 WPM  CQ      0237Z\r\n";

struct Feed {
    events: mpsc::UnboundedReceiver<FeedEvent>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

fn spawn_feed(port: u16, clock: Clock) -> Feed {
    let (tx, events) = mpsc::unbounded_channel();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let cfg = FeedConfig {
        host: "127.0.0.1".to_string(),
        port,
        login_callsign: LOGIN.to_string(),
    };
    let task = tokio::spawn(feed::run(
        cfg,
        clock,
        move |ev| {
            let _ = tx.send(ev);
        },
        Arc::new(Metrics::new()),
        shutdown_rx,
    ));
    Feed {
        events,
        shutdown,
        task,
    }
}

fn fixed_clock(t: Arc<AtomicI64>) -> Clock {
    Arc::new(move || t.load(Ordering::SeqCst))
}

/// Unix seconds that advance with tokio's (possibly paused) clock.
fn tokio_clock(base: i64) -> Clock {
    let start = Instant::now();
    Arc::new(move || base + start.elapsed().as_secs() as i64)
}

/// Reads until the client's CRLF-terminated login arrives (or EOF).
async fn read_login(sock: &mut TcpStream) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while !got.ends_with(b"\r\n") {
        let n = sock.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// Accepts one connection, sends `prompt`, and returns the socket once the
/// client's login has arrived.
async fn accept_and_login(listener: &TcpListener, prompt: &[u8]) -> TcpStream {
    let (mut sock, _) = listener.accept().await.unwrap();
    sock.write_all(prompt).await.unwrap();
    let login = read_login(&mut sock).await;
    assert_eq!(login, format!("{LOGIN}\r\n").as_bytes());
    sock
}

async fn next_event(feed: &mut Feed, within: Duration) -> FeedEvent {
    tokio::time::timeout(within, feed.events.recv())
        .await
        .expect("timed out waiting for a feed event")
        .expect("feed task ended")
}

async fn assert_stops_within(feed: Feed, within: Duration) {
    let _ = feed.shutdown.send(true);
    tokio::time::timeout(within, feed.task)
        .await
        .expect("feed::run did not return promptly after shutdown")
        .unwrap();
}

#[tokio::test]
async fn logs_in_after_a_prompt_without_newline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    let (mut sock, _) = listener.accept().await.unwrap();
    sock.write_all(b"Please enter your call: ").await.unwrap();
    let login = tokio::time::timeout(Duration::from_secs(1), read_login(&mut sock))
        .await
        .expect("no login within 1 s of a newline-less prompt");
    assert_eq!(login, b"W3XYZ-1\r\n");

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn logs_in_after_a_newline_terminated_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    let (mut sock, _) = listener.accept().await.unwrap();
    // No colon: only the line end can trigger the login.
    sock.write_all(b"Please enter your call\r\n").await.unwrap();
    let login = tokio::time::timeout(Duration::from_secs(1), read_login(&mut sock))
        .await
        .expect("no login within 1 s of a newline-terminated prompt");
    assert_eq!(login, b"W3XYZ-1\r\n");

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

#[tokio::test(start_paused = true)]
async fn logs_in_after_10_s_of_silence() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    // Measured from before the accept: paused time auto-advances whenever
    // every task waits, including on socket I/O, so the clock may already
    // have moved by the time `accept` returns.
    let start = Instant::now();
    let (mut sock, _) = listener.accept().await.unwrap();
    // Send nothing at all.
    let login = tokio::time::timeout(Duration::from_secs(60), read_login(&mut sock))
        .await
        .expect("no login after a silent server");
    assert_eq!(login, b"W3XYZ-1\r\n");
    let waited = start.elapsed();
    assert!(
        waited >= LOGIN_PROMPT_WAIT,
        "login went out after {waited:?}, before {LOGIN_PROMPT_WAIT:?} of silence"
    );

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn sends_nothing_but_the_login() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    let (mut sock, _) = listener.accept().await.unwrap();
    sock.write_all(b"Please enter your call: ").await.unwrap();
    let mut received = read_login(&mut sock).await;
    for _ in 0..50 {
        sock.write_all(CW_LINE.as_bytes()).await.unwrap();
    }
    // Every line processed: Up plus 50 reports.
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { .. }
    ));
    for _ in 0..50 {
        assert!(matches!(
            next_event(&mut feed, Duration::from_secs(5)).await,
            FeedEvent::Report(_)
        ));
    }
    // Hold the connection open a little longer, then end the session and
    // collect everything the client ever wrote, up to its close.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_stops_within(feed, Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut received))
        .await
        .expect("client did not close after shutdown")
        .unwrap();
    assert_eq!(received, b"W3XYZ-1\r\n");
}

#[tokio::test]
async fn delivers_cw_reports_with_arrival_time_from_the_injected_clock() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let now = Arc::new(AtomicI64::new(1_760_000_000));
    let mut feed = spawn_feed(
        listener.local_addr().unwrap().port(),
        fixed_clock(now.clone()),
    );

    let mut sock = accept_and_login(&listener, b"Please enter your call: ").await;
    // A banner line after login is not a spot: no Up yet.
    sock.write_all(b"Hello W3XYZ-1, this is the RBN relay\r\n")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), feed.events.recv())
            .await
            .is_err(),
        "a non-DX line produced an event"
    );

    // The first parsed DX line (here RTTY) brings the feed Up, at its
    // arrival time, without delivering the RTTY report.
    now.store(1_760_000_100, Ordering::SeqCst);
    sock.write_all(RTTY_LINE.as_bytes()).await.unwrap();
    assert_eq!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { at: 1_760_000_100 }
    );

    now.store(1_760_000_105, Ordering::SeqCst);
    sock.write_all(format!("{FT8_LINE}{CW_LINE}").as_bytes())
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(RbnReport {
            spotter: "S53A".to_string(),
            call: "N8II".to_string(),
            freq_hz: 14_011.90 * 1000.0,
            wpm: Some(25.0),
            time: 1_760_000_105,
        })
    );

    now.store(1_760_000_111, Ordering::SeqCst);
    sock.write_all(CW_LINE_2.as_bytes()).await.unwrap();
    assert_eq!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(RbnReport {
            spotter: "KM3T-2".to_string(),
            call: "JA1ABC".to_string(),
            freq_hz: 7_023.40 * 1000.0,
            wpm: Some(31.0),
            time: 1_760_000_111,
        })
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), feed.events.recv())
            .await
            .is_err(),
        "a non-CW line was delivered"
    );

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

/// Paused time for the silent minute only: with the clock paused, tokio
/// auto-advances to the next timer whenever every task is waiting, socket
/// I/O included, so a connect or login in flight could see its timeouts
/// fire early. The connection is set up in real time; the clock is paused
/// once the feed sits idle (nothing in flight), and resumed for the
/// reconnect.
#[tokio::test]
async fn idle_feed_reconnects_after_60_s() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut feed = spawn_feed(
        listener.local_addr().unwrap().port(),
        tokio_clock(1_760_000_000),
    );

    let mut first = accept_and_login(&listener, b"login: ").await;
    first.write_all(CW_LINE.as_bytes()).await.unwrap();
    let FeedEvent::Up { at: up_at } = next_event(&mut feed, Duration::from_secs(5)).await else {
        panic!("expected Up first");
    };
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(_)
    ));

    // Silence, with the connection held open (`first` stays alive).
    tokio::time::pause();
    let silent_from = tokio::time::Instant::now();
    let FeedEvent::Down { at: down_at } = next_event(&mut feed, Duration::from_secs(120)).await
    else {
        panic!("expected Down after an idle feed");
    };
    let silent_for = silent_from.elapsed();
    tokio::time::resume();
    assert!(
        silent_for >= FEED_IDLE_TIMEOUT,
        "Down came {silent_for:?} after the last line"
    );
    // Stamped with the last line's time, not the timeout's: the silent
    // minute delivered nothing, so it must not count as covered.
    assert_eq!(down_at, up_at, "Down is stamped at the last line received");

    // The reconnect: a second connection, and Up again on its first line.
    let mut second = tokio::time::timeout(
        Duration::from_secs(10),
        accept_and_login(&listener, b"login: "),
    )
    .await
    .expect("feed did not reconnect after the idle timeout");
    second.write_all(CW_LINE.as_bytes()).await.unwrap();
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { .. }
    ));
    drop(first);

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

/// Real time (about 4 s): paused time auto-advances through the client's
/// connect and login timeouts while socket I/O is in flight, which turns
/// the measured gaps into noise.
#[tokio::test]
async fn reconnects_with_backoff_after_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    // Sessions that close before any spot: the never-connected ladder
    // sleeps 1 s, then 2 s (`backoff.rs`; it goes on 4, 8 ... 60).
    let (sock, _) = listener.accept().await.unwrap();
    let mut last = Instant::now();
    drop(sock);
    let (sock, _) = listener.accept().await.unwrap();
    let gap = last.elapsed();
    assert!(
        gap >= Duration::from_secs(1) && gap < Duration::from_secs(2),
        "expected the 1 s first rung, got {gap:?}"
    );
    last = Instant::now();
    drop(sock);
    let (mut sock, _) = listener.accept().await.unwrap();
    let gap = last.elapsed();
    assert!(
        gap >= Duration::from_secs(2) && gap < Duration::from_secs(3),
        "expected the 2 s second rung, got {gap:?}"
    );

    // A session that reached Up resets the ladder: the next attempt
    // follows after 1 s, not 4 s.
    sock.write_all(b"login: ").await.unwrap();
    read_login(&mut sock).await;
    sock.write_all(CW_LINE.as_bytes()).await.unwrap();
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { .. }
    ));
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(_)
    ));
    last = Instant::now();
    drop(sock);
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Down { .. }
    ));
    let (_sock, _) = listener.accept().await.unwrap();
    let gap = last.elapsed();
    assert!(
        gap >= Duration::from_secs(1) && gap < Duration::from_secs(2),
        "expected the 1 s reset rung after a session that was up, got {gap:?}"
    );

    assert_stops_within(feed, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn shutdown_interrupts_connect_login_and_read() {
    // Connect: a listener whose accept queue is full, so a further SYN
    // goes unanswered (Linux, macOS). Where the platform refuses instead,
    // the client is in its backoff sleep, which must stop just as fast.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let full = socket.listen(0).unwrap();
    let full_port = full.local_addr().unwrap().port();
    let mut fillers = Vec::new();
    for _ in 0..3 {
        if let Ok(Ok(s)) = tokio::time::timeout(
            Duration::from_millis(200),
            TcpStream::connect(("127.0.0.1", full_port)),
        )
        .await
        {
            fillers.push(s);
        }
    }
    let feed = spawn_feed(full_port, tokio_clock(0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_stops_within(feed, Duration::from_secs(2)).await;
    drop(fillers);
    drop(full);

    // Login: the server accepts and never prompts; shutdown lands inside
    // the 10 s silence wait, and the login is never sent.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));
    let (mut sock, _) = listener.accept().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_stops_within(feed, Duration::from_secs(2)).await;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut received))
        .await
        .expect("client did not close after shutdown")
        .unwrap();
    assert!(received.is_empty(), "sent {received:?} after shutdown");

    // Read: logged in and up; shutdown ends the session without a Down.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));
    let mut sock = accept_and_login(&listener, b"login: ").await;
    sock.write_all(CW_LINE.as_bytes()).await.unwrap();
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { .. }
    ));
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(_)
    ));
    let _ = feed.shutdown.send(true);
    tokio::time::timeout(Duration::from_secs(2), feed.task)
        .await
        .expect("feed::run did not return promptly after shutdown")
        .unwrap();
    assert_eq!(
        feed.events.try_recv().ok(),
        None,
        "shutdown is not a feed loss"
    );
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut rest))
        .await
        .expect("client did not close after shutdown")
        .unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn an_oversized_line_reconnects_instead_of_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut feed = spawn_feed(listener.local_addr().unwrap().port(), tokio_clock(0));

    let mut sock = accept_and_login(&listener, b"login: ").await;
    sock.write_all(CW_LINE.as_bytes()).await.unwrap();
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Up { .. }
    ));
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Report(_)
    ));
    // Past bounded_io::MAX_LINE_BYTES (1024), no newline.
    let oversized = vec![b'A'; manta_server::bounded_io::MAX_LINE_BYTES * 2];
    sock.write_all(&oversized).await.unwrap();
    assert!(matches!(
        next_event(&mut feed, Duration::from_secs(5)).await,
        FeedEvent::Down { .. }
    ));
    let _again = tokio::time::timeout(
        Duration::from_secs(5),
        accept_and_login(&listener, b"login: "),
    )
    .await
    .expect("feed did not reconnect after an oversized line");
    drop(sock);

    assert_stops_within(feed, Duration::from_secs(2)).await;
}
