//! MAN-318 acceptance: with `[callsign_lookup]` on, a live `manta run`
//! withholds a call QRZ.com does not list, spots anyway when QRZ.com does
//! not answer within 10 s, and a file replay looks nothing up. The live
//! source is MAN-140's fake radiod streaming V1 (W1AW); QRZ.com is a
//! loopback mock over `http://127.0.0.1` (the loader allows plain HTTP to
//! loopback only). Gated like `ka9q_input.rs`.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::ka9q::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How the mock answers a lookup (a login always gets a key).
#[derive(Clone, Copy)]
enum Behavior {
    /// `Not found: W1AW`.
    NotFound,
    /// Hold the socket open and answer nothing.
    SilentLookups,
}

const LOGIN_OK: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session><Key>k3y0001</Key><Count>1</Count><SubExp>Wed Jan 1 12:34:03 2027</SubExp></Session>
</QRZDatabase>"#;
const NOT_FOUND: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session><Error>Not found: W1AW</Error><Key>k3y0001</Key></Session>
</QRZDatabase>"#;

/// A loopback QRZ.com XML service: one thread per connection; records each
/// request's line, body and arrival time.
struct QrzMock {
    port: u16,
    requests: Arc<Mutex<Vec<(String, String, Instant)>>>,
    accepted: Arc<Mutex<usize>>,
}

impl QrzMock {
    fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(Mutex::new(0));
        let (seen, count) = (requests.clone(), accepted.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                *count.lock().unwrap() += 1;
                let seen = seen.clone();
                thread::spawn(move || serve(stream, behavior, &seen));
            }
        });
        Self {
            port,
            requests,
            accepted,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/xml/current/", self.port)
    }

    fn seen(&self) -> Vec<(String, String, Instant)> {
        self.requests.lock().unwrap().clone()
    }

    fn lookups(&self) -> Vec<(String, String, Instant)> {
        self.seen()
            .into_iter()
            .filter(|(_, body, _)| body.starts_with("s="))
            .collect()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    let line = head.lines().next().unwrap_or_default().to_string();
    Some((line, String::from_utf8_lossy(&body).to_string()))
}

fn serve(mut stream: TcpStream, behavior: Behavior, seen: &Mutex<Vec<(String, String, Instant)>>) {
    while let Some((line, body)) = read_request(&mut stream) {
        let login = body.starts_with("username=");
        seen.lock().unwrap().push((line, body, Instant::now()));
        let reply = match (login, behavior) {
            (true, _) => LOGIN_OK,
            (false, Behavior::NotFound) => NOT_FOUND,
            (false, Behavior::SilentLookups) => {
                let mut sink = Vec::new();
                let _ = stream.read_to_end(&mut sink);
                return;
            }
        };
        let bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{reply}",
            reply.len()
        );
        if stream.write_all(bytes.as_bytes()).is_err() {
            return;
        }
    }
}

fn lookup_table(mock: &QrzMock) -> String {
    format!(
        "[callsign_lookup]\nenabled = true\nqrz_username = \"N1TEST\"\nqrz_password = \
         \"p&ss;w=rd\"\nqrz_url = \"{}\"\n",
        mock.url()
    )
}

/// A live `run` on a fake radiod looping V1 at 2x real time.
struct LiveRun {
    _fake: FakeRadiod,
    _child: ChildGuard,
    metrics_port: u16,
    err_lines: Arc<Mutex<Vec<String>>>,
    /// Each spot's JSON and when stdout carried it.
    spot_rx: mpsc::Receiver<(serde_json::Value, Instant)>,
    done_rx: mpsc::Receiver<()>,
}

impl LiveRun {
    fn start(dir: &Path, mock: &QrzMock) -> Self {
        let wav = v1_fixture(dir);
        let samples = Arc::new(wav_samples(&wav));
        let group = unique_group();
        let data_port = free_udp_port();
        let mut fake = FakeRadiod::status_only(group, channel_status(group, data_port, false));
        let [telnet_port, json_port, metrics_port] = free_ports();
        let cfg = dir.join("manta.toml");
        std::fs::write(
            &cfg,
            format!(
                "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                 metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet_port}\n\
                 json_port = {json_port}\nmetrics_port = {metrics_port}\n\n{}",
                lookup_table(mock)
            ),
        )
        .unwrap();
        let mut child = manta()
            .args(["run", "--ka9q-group"])
            .arg(group.to_string())
            .args(["--ka9q-port", &data_port.to_string(), "--json", "--config"])
            .arg(&cfg)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let guard = ChildGuard(child);
        let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        {
            let err_lines = err_lines.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.contains(READY_MARKER) {
                        let _ = ready_tx.send(());
                    }
                    err_lines.lock().unwrap().push(line);
                }
            });
        }
        let (spot_tx, spot_rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(s) = v.get("spot") {
                        let _ = spot_tx.send((s.clone(), Instant::now()));
                    }
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                panic!(
                    "manta never printed {READY_MARKER:?}; stderr:\n{}",
                    err_lines.lock().unwrap().join("\n")
                )
            });
        // Looping, so batches keep running (and releasing held spots)
        // after the first pass.
        let (done_tx, done_rx) = mpsc::channel();
        fake.stream(group, data_port, samples, 2.0, None, true, done_tx);
        Self {
            _fake: fake,
            _child: guard,
            metrics_port,
            err_lines,
            spot_rx,
            done_rx,
        }
    }

    fn stderr(&self) -> String {
        self.err_lines.lock().unwrap().join("\n")
    }

    /// Polls `/metrics` until `ready` holds or `budget` passes.
    fn metrics_until(&self, budget: Duration, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + budget;
        loop {
            let body = scrape_metrics(self.metrics_port).unwrap_or_default();
            if ready(&body) || Instant::now() >= deadline {
                return body;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Gherkin (MAN-318, scenario 1):
///   Given callsign lookup is enabled with the service's QRZ credentials
///   And a candidate callsign that QRZ reports as not found
///   When the candidate would otherwise be spotted
///   Then it is not spotted
///   And the reason is recorded
#[test]
fn scenario_1_a_call_qrz_does_not_list_is_not_spotted() {
    let dir = tempfile::tempdir().unwrap();
    let mock = QrzMock::start(Behavior::NotFound);
    let run = LiveRun::start(dir.path(), &mock);
    let body = run.metrics_until(Duration::from_secs(60), |b| {
        metric_value(b, "manta_callsign_lookup_withheld_total") == Some(1)
    });
    assert_eq!(
        metric_value(&body, "manta_callsign_lookup_withheld_total"),
        Some(1),
        "{body}\nstderr:\n{}",
        run.stderr()
    );
    run.done_rx.recv_timeout(Duration::from_secs(60)).unwrap();
    thread::sleep(Duration::from_secs(3));
    assert!(
        run.spot_rx.try_recv().is_err(),
        "a call QRZ.com does not list was spotted"
    );
    let body = scrape_metrics(run.metrics_port).unwrap_or_default();
    for (metric, want) in [
        (r#"manta_callsign_lookups_total{outcome="not_found"}"#, 1),
        (r#"manta_callsign_lookups_total{outcome="found"}"#, 0),
        (r#"manta_callsign_lookups_total{outcome="failed"}"#, 0),
        ("manta_callsign_lookup_withheld_total", 1),
        ("manta_callsign_lookup_unverified_total", 0),
        ("manta_spots_total", 0),
    ] {
        assert_eq!(metric_value(&body, metric), Some(want), "{metric}\n{body}");
    }
    let err = run.stderr();
    assert!(
        err.contains(
            "callsign lookup: W1AW not spotted: QRZ.com does not list it (answer kept 24 h)"
        ),
        "{err}"
    );
    let seen = mock.seen();
    let logins: Vec<_> = seen
        .iter()
        .filter(|(_, b, _)| b.starts_with("username="))
        .collect();
    assert_eq!(logins.len(), 1, "{seen:?}");
    assert_eq!(mock.lookups().len(), 1, "{seen:?}");
    assert!(
        logins[0]
            .1
            .contains("username=N1TEST&password=p%26ss%3Bw%3Drd&agent=manta%2F"),
        "{}",
        logins[0].1
    );
    assert_eq!(mock.lookups()[0].1, "s=k3y0001&callsign=W1AW");
    for (line, _, _) in &seen {
        assert!(!line.contains("password") && !line.contains("s="), "{line}");
    }
}

/// Gherkin (MAN-318, scenario 2):
///   Given callsign lookup is enabled
///   When QRZ does not answer within 10 seconds
///   Then the spot is emitted anyway
///   And the lookup failure is counted
#[test]
fn scenario_2_an_unanswered_lookup_spots_anyway_and_counts_the_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mock = QrzMock::start(Behavior::SilentLookups);
    let run = LiveRun::start(dir.path(), &mock);
    let (spot, at) = run
        .spot_rx
        .recv_timeout(Duration::from_secs(90))
        .unwrap_or_else(|_| panic!("no spot within 90 s; stderr:\n{}", run.stderr()));
    assert_eq!(spot["dxCall"].as_str(), Some("W1AW"));
    let lookups = mock.lookups();
    assert_eq!(lookups.len(), 1, "{:?}", mock.seen());
    let held = at.duration_since(lookups[0].2);
    assert!(
        held >= Duration::from_secs(9) && held <= Duration::from_secs(25),
        "spot published {held:?} after the lookup reached QRZ.com"
    );
    let body = run.metrics_until(Duration::from_secs(20), |b| {
        metric_value(b, r#"manta_callsign_lookups_total{outcome="failed"}"#) == Some(1)
    });
    for (metric, want) in [
        (r#"manta_callsign_lookups_total{outcome="failed"}"#, 1),
        ("manta_callsign_lookup_unverified_total", 1),
        ("manta_callsign_lookup_withheld_total", 0),
        ("manta_spots_total", 1),
    ] {
        assert_eq!(metric_value(&body, metric), Some(want), "{metric}\n{body}");
    }
    let err = run.stderr();
    assert!(
        err.contains("callsign lookup: QRZ.com did not answer within 10 s"),
        "{err}"
    );
}

#[test]
fn file_replay_looks_nothing_up_and_stays_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let mock = QrzMock::start(Behavior::NotFound);
    let run_with = |table: &str| {
        let cfg = dir.path().join("replay.toml");
        std::fs::write(&cfg, table).unwrap();
        let out = manta()
            .args(["run", "--source"])
            .arg(&wav)
            .args(["--source-iq", "--json", "--config"])
            .arg(&cfg)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    let with = run_with(&lookup_table(&mock));
    let without = run_with("");
    let err = String::from_utf8_lossy(&with.stderr);
    assert!(
        err.contains("note: callsign lookup is off for file replay, so the run is repeatable"),
        "{err}"
    );
    assert_eq!(
        *mock.accepted.lock().unwrap(),
        0,
        "the replay reached QRZ.com"
    );
    assert!(!with.stdout.is_empty());
    assert_eq!(with.stdout, without.stdout);
}

#[test]
fn config_check_prints_the_table_without_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let mock = QrzMock::start(Behavior::NotFound);
    let cfg = dir.path().join("manta.toml");
    std::fs::write(&cfg, lookup_table(&mock)).unwrap();
    let out = manta()
        .args(["config", "check", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l
            == format!(
                "callsign_lookup: enabled=true qrz_username=N1TEST qrz_password=set qrz_url={}",
                mock.url()
            )),
        "{stdout}"
    );
    assert!(!stdout.contains("p&ss"), "{stdout}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("p&ss"));
}
