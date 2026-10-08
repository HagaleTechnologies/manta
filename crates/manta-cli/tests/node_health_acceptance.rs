//! MAN-128 end-to-end acceptance: all four Gherkin scenarios against a real
//! `manta run` child process, decoding a synthetic CW WAV with two dead RBN
//! uplink targets configured.
//!
//!   Scenario: Per-band spot rate is visible
//!   Scenario: A health endpoint reflects real status
//!   Scenario: Decode latency is observable
//!   Scenario: Each configured uplink target is labeled individually
//!
//! The 503-on-listener-down and decode-stopped transitions are covered
//! deterministically by `metrics_acceptance.rs`'s TCP tests; this test only
//! checks the steady-state "decoding fine" path, which is what a real
//! liveness probe sees for nearly all of a node's life.

use manta_testkit::scene::{render_scene, SignalSpec};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills and reaps the daemon child process on drop, including on a
/// failing assertion, so a failing assert never leaks a `manta` process
/// into the CI runner.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `GET <path>` over a raw TCP connection, generalized from
/// `hpsdr_metrics_acceptance.rs`'s `scrape_metrics` to cover `/healthz` too.
/// Returns `(status_line, body)`.
fn http_get(port: u16, path: &str) -> Option<(String, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .ok()?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .ok()?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp).ok()?;
    let mut parts = resp.splitn(2, "\r\n\r\n");
    let head = parts.next()?;
    let body = parts.next().unwrap_or("").to_string();
    let status_line = head.lines().next()?.to_string();
    Some((status_line, body))
}

/// The value trailing a Prometheus sample line whose text starts with
/// `metric_with_labels` (e.g. `manta_spots_by_band_total{band="20m",type="cq"}`).
fn metric_value(body: &str, metric_with_labels: &str) -> Option<f64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find(|l| l.starts_with(prefix.as_str()))
        .and_then(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

/// Synthesize a 300 s, 48 kHz mono 16-bit WAV of looping keyed CW
/// ("CQ CQ DE W1AW W1AW K" at 700 Hz audio offset, 20 WPM, mild AWGN),
/// suitable for `manta run --source` (the default mono real-audio
/// interpretation, Hilbert-transformed by `AudioIqSource`). 300 s is long
/// enough to leave a several-second scrape window even though the daemon
/// decodes much faster than real time (a 240 s recording finished in
/// roughly 4-5 s during this ticket's planning session).
fn write_cw_fixture(path: &std::path::Path) {
    let fs = 48_000.0;
    let duration_s = 300.0;
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".to_string(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 700.0,
        snr_2500_db: 25.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (scene, _texts) = render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(42))
        .expect("render synthetic CW scene");

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: fs as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create CW fixture WAV");
    for c in &scene {
        let v = (c.re * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        writer.write_sample(v).expect("write CW fixture sample");
    }
    writer.finalize().expect("finalize CW fixture WAV");
}

#[test]
fn node_health_is_observable_from_metrics_and_healthz_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let wav_path = tmp.path().join("cw48k.wav");
    write_cw_fixture(&wav_path);

    let telnet_port = free_port();
    let json_port = free_port();
    let metrics_port = free_port();
    let uplink1_port = free_port();
    let uplink2_port = free_port();

    let mut cfg_file = tempfile::NamedTempFile::new().unwrap();
    write!(
        cfg_file,
        r#"
        [server]
        station_callsign = "W1AW"
        bind_addr = "127.0.0.1"
        telnet_port = {telnet_port}
        json_port = {json_port}
        metrics_port = {metrics_port}

        [[rbn_uplink]]
        enabled = true
        target_host = "127.0.0.1"
        target_port = {uplink1_port}

        [[rbn_uplink]]
        enabled = true
        target_host = "127.0.0.1"
        target_port = {uplink2_port}
        "#
    )
    .unwrap();
    cfg_file.flush().unwrap();

    let stderr_path = tmp.path().join("manta.stderr.log");
    let child = Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("run")
        .arg("--source")
        .arg(&wav_path)
        .arg("--dial-freq-hz")
        .arg("14025000")
        .arg("--config")
        .arg(cfg_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(&stderr_path).unwrap()))
        .spawn()
        .expect("spawn manta run child process");
    let _child_guard = ChildGuard(child);

    let spot_metric = r#"manta_spots_by_band_total{band="20m",type="cq"}"#;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_metrics_body = String::new();
    loop {
        if let Some((_, body)) = http_get(metrics_port, "/metrics") {
            let seen = metric_value(&body, spot_metric).unwrap_or(0.0);
            last_metrics_body = body;
            if seen >= 1.0 {
                break;
            }
        }
        if Instant::now() >= deadline {
            let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!(
                "no 20m CQ spot became visible within 20s; last scrape:\n{last_metrics_body}\n\
                 child stderr:\n{stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Scenario: Per-band spot rate is visible.
    assert!(
        metric_value(&last_metrics_body, spot_metric).unwrap_or(0.0) >= 1.0,
        "body: {last_metrics_body}"
    );

    // Scenario: Decode latency is observable (histogram, uptime, build-info).
    assert!(last_metrics_body.contains("# TYPE manta_decode_latency_seconds histogram"));
    let count = metric_value(&last_metrics_body, "manta_decode_latency_seconds_count")
        .expect("decode latency count present");
    assert!(count >= 1.0, "body: {last_metrics_body}");
    let inf_bucket = metric_value(
        &last_metrics_body,
        r#"manta_decode_latency_seconds_bucket{le="+Inf"}"#,
    )
    .expect("+Inf bucket present");
    assert_eq!(inf_bucket, count, "body: {last_metrics_body}");
    assert!(metric_value(&last_metrics_body, "manta_uptime_seconds").is_some());
    assert!(metric_value(&last_metrics_body, "manta_start_time_seconds").is_some());
    let build_info_prefix = format!(
        r#"manta_build_info{{version="{}","#,
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        last_metrics_body
            .lines()
            .any(|l| l.starts_with(&build_info_prefix) && l.trim_end().ends_with(" 1")),
        "body: {last_metrics_body}"
    );

    // Scenario: Each configured uplink target is labeled individually.
    let target1 = format!("127.0.0.1:{uplink1_port}");
    let target2 = format!("127.0.0.1:{uplink2_port}");
    assert_eq!(
        metric_value(
            &last_metrics_body,
            &format!(r#"manta_uplink_target_connected{{target="{target1}"}}"#)
        ),
        Some(0.0),
        "body: {last_metrics_body}"
    );
    assert_eq!(
        metric_value(
            &last_metrics_body,
            &format!(r#"manta_uplink_target_connected{{target="{target2}"}}"#)
        ),
        Some(0.0),
        "body: {last_metrics_body}"
    );
    assert!(
        metric_value(
            &last_metrics_body,
            &format!(r#"manta_uplink_target_reconnects_total{{target="{target1}"}}"#)
        )
        .is_some(),
        "body: {last_metrics_body}"
    );
    assert!(
        metric_value(
            &last_metrics_body,
            &format!(r#"manta_uplink_target_reconnects_total{{target="{target2}"}}"#)
        )
        .is_some(),
        "body: {last_metrics_body}"
    );

    // Scenario: A health endpoint reflects real status -- the daemon is
    // decoding fine (only the two unreachable uplinks are unhealthy, and
    // the uplink is deliberately excluded from `/healthz`'s verdict).
    let (status_line, healthz_body) =
        http_get(metrics_port, "/healthz").expect("GET /healthz over TCP");
    assert_eq!(status_line, "HTTP/1.1 200 OK", "body: {healthz_body}");
    assert_eq!(healthz_body.lines().next(), Some("ok"));
}
