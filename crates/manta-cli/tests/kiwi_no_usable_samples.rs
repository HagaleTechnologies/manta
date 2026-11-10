//! MAN-533: a KiwiSDR that keeps its connection open but sends only frames
//! manta cannot use (unknown-tag binary frames) is dropped and reopened
//! after 30 s of reading with no usable sample, and `/status` names the
//! reason. Runs at production durations (about 35 to 40 s): no test knob
//! reaches the window from outside the process.
mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::Message;

/// The whole test's bound: the 30 s window, one backoff step and slack.
const WAIT: Duration = Duration::from_secs(90);

/// An unknown-tag binary frame: neither `SND` nor `MSG`.
const UNKNOWN_FRAME: &[u8] = b"XYZ\x00\x01\x02\x03";

/// Serves every connection until `done`, the `serve_reconnects` pattern of
/// `kiwi_adversarial.rs`: each session completes the handshake, then sends
/// only unknown-tag frames every 200 ms until `done` or a send fails. It
/// never hangs up first (an early close with unread client data becomes a
/// reset, which would end the connection for a different reason).
fn serve_unknown_tags(listener: &TcpListener, done: &AtomicBool, accepted: &AtomicUsize) {
    let deadline = Instant::now() + WAIT;
    while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
        match common::accept_kiwi_session(listener, done) {
            Ok(mut ws) => {
                accepted.fetch_add(1, Ordering::Relaxed);
                while !done.load(Ordering::Relaxed) {
                    if ws.send(Message::binary(UNKNOWN_FRAME.to_vec())).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Err(_) if done.load(Ordering::Relaxed) => break,
            Err(e) => {
                eprintln!("fake KiwiSDR: skipped a connection: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The body of `GET <path>` on the metrics listener, or `None` while it is
/// not answering yet.
fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

/// The value of a `metric{labels}` line in a Prometheus text body.
fn metric_value(body: &str, metric_with_labels: &str) -> Option<u64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

/// The `/status` `sources[]` row named `kiwi`, once it reports a loss.
fn kiwi_loss_row(port: u16) -> Option<serde_json::Value> {
    let body = http_get(port, "/status")?;
    let doc: serde_json::Value = serde_json::from_str(&body).ok()?;
    let row = doc["sources"]
        .as_array()?
        .iter()
        .find(|r| r["name"] == "kiwi")?
        .clone();
    (row["losses_total"].as_u64()? >= 1).then_some(row)
}

#[test]
fn a_kiwi_sending_only_unknown_tags_is_reconnected_with_reason_no_usable_samples() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicUsize::new(0));
    let fake = {
        let (done, accepted) = (done.clone(), accepted.clone());
        std::thread::spawn(move || serve_unknown_tags(&listener, &done, &accepted))
    };

    let [telnet, json, metrics] = common::free_ports::<3>();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\n\
             metrics_port = {metrics}\n"
        ),
    )
    .unwrap();
    let mut command = common::manta();
    let mut child = command
        .env("RUST_LOG", "info")
        .args(["run", "--kiwi-host", "127.0.0.1", "--kiwi-port"])
        .arg(port.to_string())
        .args(["--kiwi-freq-hz", "14025000", "--config"])
        .arg(&cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = common::line_channel(child.stderr.take().unwrap());
    let child = common::KillOnDrop(child);
    let started = Instant::now();

    common::expect_line(&stderr, &["manta: listening;"], Duration::from_secs(30));
    let listening = Instant::now();

    // 1. The window, not something earlier, ended the connection.
    let (lost, before) = common::expect_line_all(
        &stderr,
        &["source kiwi lost:", "no usable samples"],
        WAIT.saturating_sub(started.elapsed()),
    );
    let after = listening.elapsed();
    assert!(
        after >= Duration::from_secs(28),
        "the connection ended after {after:?}, before the 30 s window: {lost}"
    );
    assert!(
        !before.iter().any(|l| l.contains(" lost: ")),
        "an earlier loss ended the connection first: {before:?}"
    );

    // 2. The fake sees the reopen.
    while accepted.load(Ordering::Relaxed) < 2 {
        assert!(
            started.elapsed() < WAIT,
            "the daemon never reopened the connection (accepted {})",
            accepted.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // 3. `/status` names the reason.
    let row = loop {
        if let Some(row) = kiwi_loss_row(metrics) {
            break row;
        }
        assert!(
            started.elapsed() < WAIT,
            "/status never showed the kiwi loss"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(row["last_loss_reason"], "no usable samples", "{row}");
    assert!(row["losses_total"].as_u64().unwrap() >= 1, "{row}");

    // 4. The outage and the discarded frames are counted.
    let body = loop {
        if let Some(body) = http_get(metrics, "/metrics") {
            let malformed = metric_value(
                &body,
                r#"manta_input_malformed_packets_total{source="kiwi"}"#,
            );
            if malformed.is_some_and(|n| n > 0) {
                break body;
            }
        }
        assert!(
            started.elapsed() < WAIT,
            "manta_input_malformed_packets_total{{source=\"kiwi\"}} never rose above 0"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let outages = metric_value(&body, r#"manta_source_outages_total{source="kiwi"}"#);
    assert!(
        outages.is_some_and(|n| n >= 1),
        "manta_source_outages_total{{source=\"kiwi\"}} = {outages:?}"
    );

    done.store(true, Ordering::Relaxed);
    drop(child);
    fake.join().expect("the fake KiwiSDR panicked");
}
