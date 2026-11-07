//! MAN-489: a ping-fed Kiwi outage settles a held call before resume.
mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tungstenite::Message;

#[test]
fn a_kiwi_held_tail_spots_within_five_seconds_before_same_session_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let (go, begin) = mpsc::channel();
    let (stopped, pauses) = mpsc::channel();
    let (resume, gate) = mpsc::channel();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 12_000.0;
    spec.center_freq_hz = 14_025_000.0;
    spec.duration_s = 4.0;
    spec.signals[0].wpm = 40.0;
    spec.signals[0].loop_text = false;
    let segments: Vec<_> = ["DE W1AW", "DE K1ABC"]
        .into_iter()
        .map(|text| {
            spec.signals[0].text = text.into();
            manta_testkit::vectors::render(&spec).unwrap().samples
        })
        .collect();
    let fake = std::thread::spawn(move || {
        let mut ws = common::accept_kiwi_session(&listener, &stop).unwrap();
        if begin.recv_timeout(Duration::from_secs(30)).is_err() {
            return;
        }
        let mut seq = 0u32;
        for samples in segments {
            let started = Instant::now();
            let mut last_send = started;
            for (index, block) in samples.chunks(512).enumerate() {
                let mut frame = b"SND".to_vec();
                frame.push(0x08);
                frame.extend_from_slice(&seq.to_le_bytes());
                frame.extend_from_slice(&[0; 12]);
                for sample in block {
                    for value in [sample.re, sample.im] {
                        frame.extend_from_slice(
                            &((value * 32767.0).clamp(-32768.0, 32767.0) as i16).to_be_bytes(),
                        );
                    }
                }
                ws.send(Message::binary(frame)).unwrap();
                last_send = Instant::now();
                seq = seq.wrapping_add(1);
                let due = started + Duration::from_secs_f64((index + 1) as f64 * 512.0 / 12_000.0);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
            stopped.send(last_send).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match gate.recv_timeout(Duration::from_millis(25)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if !stop.load(Ordering::Relaxed) && Instant::now() < deadline =>
                    {
                        if ws.send(Message::Ping(Vec::new().into())).is_err() {
                            return;
                        }
                    }
                    _ => return,
                }
            }
        }
        while !stop.load(Ordering::Relaxed) {
            if ws.send(Message::Ping(Vec::new().into())).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    let held: [TcpListener; 3] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    let [telnet, json, metrics] = held.each_ref().map(|l| l.local_addr().unwrap().port());
    drop(held);
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("manta.toml");
    std::fs::write(&cfg, format!("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\nmetrics_port = {metrics}\n[spot]\nallowlist = [\"W1AW\", \"K1ABC\"]\n")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(["run", "--kiwi-host", "127.0.0.1", "--kiwi-port"])
        .arg(port.to_string())
        .args(["--kiwi-freq-hz", "14025000", "--json", "--config"])
        .arg(cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let child = common::KillOnDrop(child);
    let (ready_tx, ready) = mpsc::channel();
    let (errors_tx, errors) = mpsc::channel();
    let err_reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("manta: listening;") {
                let _ = ready_tx.send(());
            }
            let _ = errors_tx.send(line);
        }
    });
    let (spot_tx, spots) = mpsc::channel();
    let out_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(s) = v.get("spot") {
                    let _ = spot_tx.send((s.clone(), Instant::now()));
                }
            }
        }
    });
    ready.recv_timeout(Duration::from_secs(30)).unwrap();
    go.send(()).unwrap();
    let mut previous_ts = None;
    for call in ["W1AW", "K1ABC"] {
        let last_send = pauses.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(spots.try_recv().is_err(), "held tail spotted before stall");
        let (spot, received) = spots
            .recv_timeout(Duration::from_secs(5).saturating_sub(last_send.elapsed()))
            .unwrap_or_else(|e| panic!("{e}: {:?}", errors.try_iter().collect::<Vec<_>>()));
        assert_eq!(spot["dxCall"], call);
        assert!(received.duration_since(last_send) < Duration::from_secs(5));
        let ts = spot["sampleTs"].as_u64().unwrap();
        assert!(previous_ts.is_none_or(|previous| ts > previous));
        previous_ts = Some(ts);
        assert!(spots.try_recv().is_err());
        assert!(!errors
            .try_iter()
            .any(|l| l.contains(" lost: ") || l.contains(" reconnected after ")));
        resume.send(()).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    drop(child);
    fake.join().unwrap();
    out_reader.join().unwrap();
    err_reader.join().unwrap();
}
