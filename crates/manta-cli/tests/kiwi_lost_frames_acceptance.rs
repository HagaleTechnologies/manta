//! MAN-529 acceptance: a KiwiSDR `seq` gap moves the spot clock by the lost
//! span, so a spot decoded 60 s after the gap carries the same `sampleTs`
//! (and so the same UTC `timestamp` under the same session start) as the
//! same signal through a gap-free stream.
//!
//! A fake KiwiSDR streams a rendered 12 kHz V1 (a 63 s noise lead-in, then
//! 12 s of W1AW at +2 kHz, then a noise tail) at 8x real time, once
//! gap-free and once with SND frames 60-62 lost, to a real `manta run`.
//! Before MAN-529 the gapped spot came out 12,288 samples (128 ms) early
//! (measured on `de39a71`).
mod common;

use num_complex::Complex32;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Speed of the fake stream against real time.
const SPEED: f64 = 8.0;
/// The SND frames the gapped run loses (512 pairs each at 12000 Hz).
const LOST: [u32; 3] = [60, 61, 62];

/// The 12 kHz stream: 63 s of noise, 12 s of V1 (`CQ CQ DE W1AW W1AW K`
/// once, +2 kHz), 10 s of noise.
fn stream_samples() -> Vec<Complex32> {
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 12_000.0;
    spec.center_freq_hz = 14_025_000.0;
    spec.signals[0].offset_hz = 2_000.0;
    spec.signals[0].loop_text = false;
    let mut noise = spec.clone();
    noise.duration_s = 63.0;
    // `render` needs a signal; at -100 dB it is noise only.
    noise.signals[0].snr_2500_db = -100.0;
    let lead = manta_testkit::vectors::render(&noise).unwrap().samples;
    spec.duration_s = 12.0;
    let cw = manta_testkit::vectors::render(&spec).unwrap().samples;
    let tail = lead[..10 * 12_000].to_vec();
    [lead, cw, tail].concat()
}

/// The first spot of one `manta run --kiwi-host ... --json` against the fake
/// streaming `samples` with the SND frames in `skip` lost, and manta's
/// stderr up to that spot.
fn first_spot(samples: Arc<Vec<Complex32>>, skip: &'static [u32]) -> (serde_json::Value, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let (go_tx, go) = mpsc::channel::<()>();
    let fake = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let Ok(mut ws) = common::accept_kiwi_session(&listener, &stop) else {
                return;
            };
            if go.recv_timeout(Duration::from_secs(60)).is_err() {
                return;
            }
            common::stream_snd_samples(&mut ws, &samples, SPEED, skip, || {
                stop.load(Ordering::Relaxed)
            });
            // Hold the connection open (silent) until the test ends.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };

    let [telnet, json, metrics] = common::free_ports();
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\nmetrics_port = {metrics}\n[spot]\nallowlist = [\"W1AW\"]\n"
        ),
    )
    .unwrap();
    let mut child = common::manta()
        .args(["run", "--kiwi-host", "127.0.0.1", "--kiwi-port"])
        .arg(port.to_string())
        .args(["--kiwi-freq-hz", "14025000", "--json", "--config"])
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let child = common::KillOnDrop(child);

    let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ready_tx, ready) = mpsc::channel();
    {
        let err_lines = err_lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("manta: listening;") {
                    let _ = ready_tx.send(());
                }
                err_lines.lock().unwrap().push(line);
            }
        });
    }
    let (spot_tx, spots) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(s) = v.get("spot") {
                    let _ = spot_tx.send(s.clone());
                }
            }
        }
    });
    let stderr_now = || err_lines.lock().unwrap().join("\n");
    ready
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("manta never became ready; stderr:\n{}", stderr_now()));
    go_tx.send(()).unwrap();
    let spot = spots
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| panic!("no spot within 120 s; stderr:\n{}", stderr_now()));
    let err = stderr_now();
    stop.store(true, Ordering::Relaxed);
    drop(child);
    fake.join().unwrap();
    (spot, err)
}

/// Gherkin (MAN-529):
///   Given a KiwiSDR source delivering 12 kHz IQ
///   And the server skips 3 frames (sequence gap)
///   When the next frame arrives
///   Then the engine receives a missing-sample report equal to the skipped span
///   And a spot decoded 60 s later carries the same UTC timestamp as the same
///   signal through a gap-free source
#[test]
fn a_kiwi_seq_gap_keeps_a_spot_60_s_later_on_the_gap_free_clock() {
    let samples = Arc::new(stream_samples());
    let (clean, clean_err) = first_spot(samples.clone(), &[]);
    let (gapped, gapped_err) = first_spot(samples, &LOST);
    for (spot, err) in [(&clean, &clean_err), (&gapped, &gapped_err)] {
        assert_eq!(spot["dxCall"].as_str(), Some("W1AW"), "{spot}\n{err}");
        assert!(!err.contains(" lost: "), "{err}");
        assert!(!err.contains(" reconnected after "), "{err}");
    }
    let clean_ts = clean["sampleTs"].as_u64().unwrap();
    let gapped_ts = gapped["sampleTs"].as_u64().unwrap();
    // The gap sits at output index 60 x 512 x 8 + 1024 (resampler delay);
    // the spot is at least 60 s of 96 kHz stream after it.
    assert!(
        clean_ts >= (60 * 512 * 8 + 1_024) + 60 * 96_000,
        "clean spot at {clean_ts} is not 60 s after the gap"
    );
    // One channelizer hop at 96 kHz is 256 samples.
    assert!(
        gapped_ts.abs_diff(clean_ts) <= 256,
        "gapped sampleTs {gapped_ts}, gap-free {clean_ts} (before MAN-529: 12,288 early)"
    );
    let (fc, fg) = (
        clean["frequency"].as_f64().unwrap(),
        gapped["frequency"].as_f64().unwrap(),
    );
    assert!((fc - fg).abs() <= 25.0, "frequency {fg} vs {fc}");
}
