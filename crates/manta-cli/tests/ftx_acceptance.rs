//! MAN-485: real daemon coverage of the opt-in FT8/FT4 IQ-to-JSON path.
#[path = "common/launch.rs"]
mod launch;
use launch::manta;

#[cfg(not(feature = "ftx"))]
#[test]
fn digital_modes_without_the_feature_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("ftx.toml");
    std::fs::write(&cfg, "[server]\nstation_callsign = \"W5AU\"\n[input]\ntype = \"file\"\npath = \"unused.wav\"\niq = true\ndigital_modes = [\"FT8\"]\n").unwrap();
    let out = manta().args(["run", "--config"]).arg(cfg).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("digital_modes needs a manta built with the ftx feature"),
        "{err}"
    );
}

#[cfg(feature = "ftx")]
mod enabled {
    use super::manta;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // The stream begins four seconds before the UTC slot boundary.
    const BOUNDARY: u64 = 1_730_462_400;

    fn fixture(dir: &Path, fs: f64) -> PathBuf {
        use manta_testkit::ftx::{add_awgn, gfsk_iq, tones, Protocol, CQ_W1AW_FN31_FT8};
        let mut iq = gfsk_iq(
            &tones(CQ_W1AW_FN31_FT8),
            fs,
            25_234.0,
            (4.5 * fs) as usize,
            (25.0 * fs) as usize,
            Protocol::Ft8,
        );
        add_awgn(&mut iq, fs, -10.0, 11);
        manta_testkit::wav::write_fixture(dir, "ft8", &iq, fs, 14_050_000.0).unwrap()
    }

    fn start(dir: &Path, wav: &Path, modes: &str) -> (KillOnDrop, Receiver<String>) {
        let cfg = dir.join("ftx.toml");
        std::fs::write(
            &cfg,
            format!(
                "[server]\nstation_callsign = \"W5AU\"\nbind_addr = \"127.0.0.1\"\n\
             telnet_port = 0\njson_port = 0\nmetrics_port = 0\nstatus_interval_secs = 0\n\
             [input]\ntype = \"file\"\npath = {:?}\niq = true\ndigital_modes = {modes}\n",
                wav.to_str().unwrap(),
            ),
        )
        .unwrap();
        let mut child = manta()
            .args(["run", "--config"])
            .arg(cfg)
            .args(["--realtime", "--replay-epoch", &(BOUNDARY - 4).to_string()])
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ansi = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                // Keep draining even when the receiver goes away.
                let _ = tx.send(ansi.replace_all(&line, "").into_owned());
            }
        });
        (KillOnDrop(child), rx)
    }

    fn startup(rx: &Receiver<String>) -> (String, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut lines = Vec::new();
        loop {
            let line = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("no startup banner: {e}; stderr: {lines:?}"));
            if line.contains("listening:") {
                return (line, lines);
            }
            lines.push(line);
        }
    }

    fn port(banner: &str, key: &str) -> u16 {
        regex::Regex::new(&format!(r"{key}=127\.0\.0\.1:(\d+)"))
            .unwrap()
            .captures(banner)
            .unwrap()[1]
            .parse()
            .unwrap()
    }

    fn metrics(port: u16) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        body
    }

    #[test]
    fn ft8_cq_on_20m_is_spotted_on_the_json_stream() {
        let dir = tempfile::tempdir().unwrap();
        let wav = fixture(dir.path(), 96_000.0);
        let (_daemon, rx) = start(dir.path(), &wav, "[\"FT8\", \"FT4\"]");
        let (banner, _) = startup(&rx);
        let stream = TcpStream::connect(("127.0.0.1", port(&banner, "json"))).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let deadline = Instant::now() + Duration::from_secs(60);
        let spot = loop {
            assert!(Instant::now() < deadline, "no FT8 spot before deadline");
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "stream ended before FT8 CQ"
            );
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["mode"] == "FT8" {
                break value;
            }
        };
        assert_eq!(spot["dxCall"], "W1AW");
        assert_eq!(spot["dxGrid"], "FN31");
        assert_eq!(spot["timestamp"], BOUNDARY);
        assert_eq!(spot["timestamp"].as_u64().unwrap() % 15, 0);
        assert!((14_075_230..=14_075_238).contains(&spot["frequency"].as_u64().unwrap()));
        assert!(spot["wpm"].is_null());
        // Publication and metric accounting occur on the decode thread.
        // Allow the counter update to complete after the socket gets the spot.
        let metric_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let body = metrics(port(&banner, "metrics"));
            if body
                .lines()
                .any(|l| l == "manta_ftx_spots_total{mode=\"FT8\"} 1")
            {
                break;
            }
            assert!(Instant::now() < metric_deadline, "{body}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_uncovered_ft4_window_is_skipped_and_the_startup_summary_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let wav = fixture(dir.path(), 48_000.0);
        let (_daemon, rx) = start(dir.path(), &wav, "[\"FT4\"]");
        let (banner, before) = startup(&rx);
        assert!(before.iter().any(|l| l == "input file: FT4 on 20m not attempted: 14080.3-14083.0 kHz is outside this stream's 14026.0-14074.0 kHz"), "{before:?}");
        let body = metrics(port(&banner, "metrics"));
        assert!(!body.contains("manta_ftx_"), "{body}");
    }
}
