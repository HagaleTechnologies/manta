//! MAN-301 end to end, in process: a fake RBN feed, a test clock, the spot
//! bus, the JSON stream on 127.0.0.1:0 and `rbn_compare::live::spawn` with a
//! 100 ms tick. Spots and feed lines are replayed in time order with the
//! test clock set to each one's time, so a feed report's arrival time is
//! exactly the time it is sent at.
//!
//! Runs on tokio's default current-thread test runtime on purpose: the feed
//! stamps a report's arrival time, counts it and hands it to the grader in
//! one poll, so once the test sees `manta_rbn_compare_feed_reports_total`
//! move it may move the clock without racing that stamp.

use manta_server::bus::SpotBus;
use manta_server::metrics::Metrics;
use manta_server::rbn_compare::feed::FeedConfig;
use manta_server::rbn_compare::grade::GradeConfig;
use manta_server::rbn_compare::live::{self, LiveConfig};
use manta_server::rbn_compare::Clock;
use manta_spot::cty::Table;
use manta_spot::{Spot, SpotType};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ-1";
/// The design section's bus: epoch 1791719990, nonce = epoch in ns.
const EPOCH: u64 = 1_791_719_990;
const NONCE: u128 = 1_791_719_990_000_000_000;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
Fed. Rep. of Germany: 14: 28: EU: 51.0: -10.0: -1.0: DL:
    DL;
";
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../manta-cli/tests/fixtures/rbn_grade"
);

/// The four records of the design section, as published.
const SCENARIO_1_MATCH: &str = r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:7:960000:W1AW","call":"W1AW","band":"20m","spotTime":1791720000,"windowClose":1791720180,"windowS":180,"grade":"match","disagreements":[],"mantaFreqHz":14025100,"mantaWpm":25,"rbnModalFreqHz":14025000,"rbnMedianWpm":24.0,"rbnSkimmers":3,"rbnReports":3,"freqOffsetHz":100,"rbnBands":["20m"],"spotterFilter":[]}"#;
const SCENARIO_2_NONE: &str = r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:9:58560000:K5XYZ","call":"K5XYZ","band":"40m","spotTime":1791720600,"windowClose":1791720780,"windowS":180,"grade":"none","disagreements":[],"mantaFreqHz":7012300,"mantaWpm":22,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":[],"spotterFilter":[]}"#;
const SCENARIO_3_DISAGREE: &str = r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:12:173760000:W1AW","call":"W1AW","band":"20m","spotTime":1791721800,"windowClose":1791721980,"windowS":180,"grade":"disagree","disagreements":["frequency","speed"],"mantaFreqHz":14026400,"mantaWpm":30,"rbnModalFreqHz":14025000,"rbnMedianWpm":20.0,"rbnSkimmers":2,"rbnReports":2,"freqOffsetHz":1400,"rbnBands":["20m"],"spotterFilter":[]}"#;
const SCENARIO_4_ELSEWHERE: &str = r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:15:260160000:DL1ABC","call":"DL1ABC","band":"20m","spotTime":1791722700,"windowClose":1791722880,"windowS":180,"grade":"elsewhere","disagreements":[],"mantaFreqHz":14030000,"mantaWpm":28,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":["40m"],"spotterFilter":[]}"#;

const WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- fixtures

/// One CW report as the fake feed sends it, at `time`.
#[derive(Clone, Debug)]
struct Report {
    time: i64,
    spotter: String,
    khz: f64,
    call: String,
    db: i32,
    wpm: u32,
}

impl Report {
    fn line(&self) -> String {
        let secs = self.time.rem_euclid(86_400);
        format!(
            "DX de {}-#:   {:>8.1}  {:<13}  CW    {:>2} dB  {:>2} WPM  CQ      {:02}{:02}Z\r\n",
            self.spotter,
            self.khz,
            self.call,
            self.db,
            self.wpm,
            secs / 3600,
            (secs % 3600) / 60
        )
    }
}

fn report(time: i64, spotter: &str, khz: f64, call: &str, wpm: u32) -> Report {
    Report {
        time,
        spotter: spotter.to_string(),
        khz,
        call: call.to_string(),
        db: 20,
        wpm,
    }
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) to Unix seconds (days-from-civil).
fn unix_of(date_time: &str) -> i64 {
    let (date, time) = date_time.split_once(' ').unwrap();
    let d: Vec<i64> = date.split('-').map(|x| x.parse().unwrap()).collect();
    let t: Vec<i64> = time.split(':').map(|x| x.parse().unwrap()).collect();
    let (y, m) = if d[1] <= 2 {
        (d[0] - 1, d[1] + 9)
    } else {
        (d[0], d[1] - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d[2] - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + t[0] * 3600 + t[1] * 60 + t[2]
}

/// The Phase 3 RBN archive's CW rows, as feed reports.
fn fixture_reports() -> Vec<Report> {
    let csv = std::fs::read_to_string(format!("{FIXTURES}/rbn-20261011.csv")).unwrap();
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
    let (c_call, c_freq, c_dx, c_db, c_date, c_speed, c_mode) = (
        col("callsign"),
        col("freq"),
        col("dx"),
        col("db"),
        col("date"),
        col("speed"),
        col("tx_mode"),
    );
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').collect::<Vec<_>>())
        .filter(|f| f[c_mode] == "CW")
        .map(|f| Report {
            time: unix_of(f[c_date]),
            spotter: f[c_call].to_string(),
            khz: f[c_freq].parse().unwrap(),
            call: f[c_dx].to_string(),
            db: f[c_db].parse().unwrap(),
            wpm: f[c_speed].parse().unwrap(),
        })
        .collect()
}

/// The Phase 3 spot archive, as the bus `Spot`s that produced it.
fn fixture_spots() -> Vec<(i64, Spot)> {
    let jsonl = std::fs::read_to_string(format!("{FIXTURES}/spots-2026-10-11.jsonl")).unwrap();
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            let id = v["id"].as_str().unwrap();
            let parts: Vec<&str> = id.split(':').collect();
            let spot = Spot {
                callsign: v["dxCall"].as_str().unwrap().to_string(),
                freq_hz: v["frequency"].as_f64().unwrap(),
                snr_db: v["snr"].as_f64().unwrap() as f32,
                wpm: v["wpm"].as_f64().unwrap() as f32,
                spot_type: SpotType::Cq,
                confidence: 0.8,
                track_id: parts[2].parse().unwrap(),
                sample_ts: parts[3].parse().unwrap(),
            };
            (v["timestamp"].as_i64().unwrap(), spot)
        })
        .collect()
}

fn fixture_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(format!("{FIXTURES}/{name}"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn grade_config(spotters: &[&str]) -> GradeConfig {
    GradeConfig {
        spotters: spotters.iter().map(|s| s.to_string()).collect(),
        exclude_spotters: BTreeSet::from(["W3XYZ-1".to_string(), "W3XYZ".to_string()]),
        ..GradeConfig::default()
    }
}

// ---------------------------------------------------------------- fake feed

enum FeedCmd {
    Line(String),
    Drop,
}

/// A fake RBN relay: prompts `Please enter your call: `, records what the
/// client writes, then sends whatever the test queues. `Drop` closes the
/// current connection; the next one is accepted the same way.
struct FakeFeed {
    port: u16,
    tx: mpsc::UnboundedSender<FeedCmd>,
    /// Bytes each connection received from the client, in order.
    received: Arc<Mutex<Vec<String>>>,
    /// Set once a connection's client closed its end.
    client_closed: Arc<Mutex<u32>>,
}

async fn fake_feed() -> FakeFeed {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, mut rx) = mpsc::unbounded_channel::<FeedCmd>();
    let received = Arc::new(Mutex::new(Vec::new()));
    let client_closed = Arc::new(Mutex::new(0u32));
    let (received2, closed2) = (received.clone(), client_closed.clone());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (mut rd, mut wr) = sock.into_split();
            if wr.write_all(b"Please enter your call: ").await.is_err() {
                continue;
            }
            received2.lock().unwrap().push(String::new());
            let slot = received2.lock().unwrap().len() - 1;
            let (rec, closed) = (received2.clone(), closed2.clone());
            let reader = tokio::spawn(async move {
                let mut buf = [0u8; 256];
                loop {
                    match rd.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            *closed.lock().unwrap() += 1;
                            return;
                        }
                        Ok(n) => {
                            rec.lock().unwrap()[slot].push_str(&String::from_utf8_lossy(&buf[..n]))
                        }
                    }
                }
            });
            loop {
                match rx.recv().await {
                    Some(FeedCmd::Line(line)) => {
                        if wr.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Some(FeedCmd::Drop) => break,
                    None => return,
                }
            }
            reader.abort();
            drop(wr);
        }
    });
    FakeFeed {
        port,
        tx,
        received,
        client_closed,
    }
}

// ---------------------------------------------------------------- harness

struct Harness {
    now: Arc<AtomicI64>,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    feed: FakeFeed,
    live_shutdown: watch::Sender<bool>,
    _json_shutdown: watch::Sender<bool>,
    live: Option<tokio::task::JoinHandle<()>>,
    /// Every line the JSON client received, in order.
    json_lines: Arc<Mutex<Vec<String>>>,
    cw_sent: u64,
    spots_published: u64,
}

fn scraped(metrics: &Metrics, series: &str) -> Option<f64> {
    metrics
        .render_prometheus_text()
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

fn grades_total(metrics: &Metrics) -> u64 {
    metrics
        .render_prometheus_text()
        .lines()
        .filter(|l| l.starts_with("manta_rbn_compare_grades_total{"))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

impl Harness {
    async fn start(spotters: &[&str], start_time: i64) -> Self {
        let now = Arc::new(AtomicI64::new(start_time));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || now.load(Ordering::SeqCst))
        };
        let bus = Arc::new(SpotBus::new(
            SAMPLE_RATE_HZ,
            SystemTime::UNIX_EPOCH + Duration::from_secs(EPOCH),
            NONCE,
        ));
        let metrics = Arc::new(Metrics::new());
        metrics.enable_rbn_compare();

        // JSON stream.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let json_addr = listener.local_addr().unwrap();
        let (json_shutdown, json_shutdown_rx) = watch::channel(false);
        let (bus2, metrics2) = (bus.clone(), metrics.clone());
        tokio::spawn(async move {
            manta_server::json_stream::serve(
                listener,
                manta_server::json_stream::JsonStreamConfig {
                    bus: bus2,
                    metrics: metrics2,
                    lists: Arc::new(manta_spot::reference::ReferenceLists::new(
                        Arc::new(Table::parse(CTY_FIXTURE)),
                        Arc::new(manta_spot::scp::Set::bundled()),
                    )),
                    station_call: STATION_CALL.to_string(),
                    station_location: None,
                    decoder_version: "manta-test".to_string(),
                    shutdown: json_shutdown_rx,
                    drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                    stream: manta_server::json_stream::StreamKind::Spots,
                },
                manta_server::tasks::new_client_tasks(),
                manta_server::tasks::new_connection_limiter(
                    manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
                ),
                manta_server::tasks::IpQuota::new(
                    manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP,
                ),
                manta_server::rate_limit::IpRateLimiter::new(
                    manta_server::json_stream::MAX_INBOUND_PINGS,
                    manta_server::json_stream::PING_RATE_WINDOW,
                ),
            )
            .await;
        });
        let json_lines = Arc::new(Mutex::new(Vec::new()));
        let mut client = BufReader::new(TcpStream::connect(json_addr).await.unwrap());
        let sink = json_lines.clone();
        tokio::spawn(async move {
            loop {
                let mut line = String::new();
                match client.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => sink.lock().unwrap().push(line),
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let feed = fake_feed().await;
        let (live_shutdown, live_shutdown_rx) = watch::channel(false);
        let handle = live::spawn(
            LiveConfig {
                feed: FeedConfig {
                    host: "127.0.0.1".to_string(),
                    port: feed.port,
                    login_callsign: STATION_CALL.to_string(),
                },
                grade: grade_config(spotters),
                station_callsign: STATION_CALL.to_string(),
                tick: Duration::from_millis(100),
            },
            bus.clone(),
            metrics.clone(),
            clock,
            live_shutdown_rx,
        );

        let h = Self {
            now,
            bus,
            metrics,
            feed,
            live_shutdown,
            _json_shutdown: json_shutdown,
            live: Some(handle),
            json_lines,
            cw_sent: 0,
            spots_published: 0,
        };
        // Logged in: the fake feed holds the login line.
        let received = h.feed.received.clone();
        wait_until("the feed login", || {
            received
                .lock()
                .unwrap()
                .first()
                .is_some_and(|s| s.ends_with('\n'))
        })
        .await;
        h
    }

    fn set_clock(&self, t: i64) {
        self.now.store(t, Ordering::SeqCst);
    }

    fn metric(&self, series: &str) -> Option<f64> {
        scraped(&self.metrics, series)
    }

    /// Sends one CW report with the clock at its time; returns once the
    /// comparator has it.
    async fn send(&mut self, r: &Report) {
        self.set_clock(r.time);
        self.feed.tx.send(FeedCmd::Line(r.line())).unwrap();
        self.cw_sent += 1;
        let want = self.cw_sent as f64;
        let metrics = self.metrics.clone();
        wait_until("the feed to accept a report", || {
            scraped(&metrics, "manta_rbn_compare_feed_reports_total") == Some(want)
        })
        .await;
    }

    /// Publishes one spot with the clock at its time; returns once the
    /// comparator has queued it.
    async fn publish(&mut self, time: i64, spot: Spot) {
        self.set_clock(time);
        self.bus.publish(spot);
        self.spots_published += 1;
        let want = self.spots_published;
        let metrics = self.metrics.clone();
        wait_until("the comparator to queue a spot", || {
            scraped(&metrics, "manta_rbn_compare_pending").unwrap_or(0.0) as u64
                + grades_total(&metrics)
                == want
        })
        .await;
    }

    fn grade_lines(&self) -> Vec<String> {
        self.json_lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("\"kind\":\"spotGrade\""))
            .map(|l| {
                let grade: serde_json::Value = serde_json::from_str(l).unwrap();
                let seq = grade["seq"].as_u64().expect("streamed grade has seq");
                let spot_seq = self
                    .bus
                    .json_resume(None, Some(0), seq)
                    .records
                    .iter()
                    .find_map(|r| match r {
                        manta_server::bus::JsonItem::Spot { seq, spot, .. }
                            if manta_server::spot_message::spot_id(
                                STATION_CALL,
                                NONCE,
                                &spot.spot,
                            ) == grade["spotId"].as_str().unwrap() =>
                        {
                            Some(*seq)
                        }
                        _ => None,
                    })
                    .expect("graded spot retained");
                assert!(seq > spot_seq);
                // Remove only seq, preserving the offline fixture's exact key order.
                l.trim_end_matches('\n')
                    .replacen(&format!("\"seq\":{seq},"), "", 1)
            })
            .collect()
    }

    fn spot_lines(&self) -> usize {
        self.json_lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| !l.contains("\"kind\""))
            .count()
    }

    async fn wait_grades(&self, n: usize, within: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let lines = self.grade_lines();
            if lines.len() >= n {
                return lines;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("expected {n} grade lines, got {lines:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Replays reports and spots in time order (reports first on a tie),
    /// then moves the clock to `end`.
    async fn replay(&mut self, reports: &[Report], spots: &[(i64, Spot)], end: i64) {
        enum Ev<'a> {
            R(&'a Report),
            S(i64, &'a Spot),
        }
        let mut events: Vec<(i64, u8, Ev)> = reports
            .iter()
            .map(|r| (r.time, 0, Ev::R(r)))
            .chain(spots.iter().map(|(t, s)| (*t, 1, Ev::S(*t, s))))
            .collect();
        events.sort_by_key(|(t, order, _)| (*t, *order));
        for (_, _, ev) in events {
            match ev {
                Ev::R(r) => self.send(r).await,
                Ev::S(t, s) => self.publish(t, s.clone()).await,
            }
        }
        self.set_clock(end);
    }

    async fn stop(mut self) {
        let _ = self.live_shutdown.send(true);
        if let Some(h) = self.live.take() {
            tokio::time::timeout(WAIT, h)
                .await
                .expect("comparator stops on shutdown")
                .unwrap();
        }
    }
}

/// A CW line for a call no scenario grades, sent first so the feed is up
/// (its first `DX de` line) well before any window opens.
fn warm_up(time: i64) -> Report {
    report(time, "K5TR", 7021.0, "N4RV", 24)
}

fn scenario_spot(n: usize) -> (i64, Spot) {
    fixture_spots().swap_remove(n)
}

/// Shared body of the four scenario tests: the spot alone, before its
/// window closes, then its grade within 1 s of a feed line arriving after
/// the close (a feed silent since inside the window may be dead, so its
/// silence alone never grades).
async fn run_scenario(spot_index: usize, reports: Vec<Report>, expected: &str) {
    let (t, spot) = scenario_spot(spot_index);
    let mut h = Harness::start(&[], t - 400).await;
    h.send(&warm_up(t - 300)).await;
    h.replay(&reports, &[(t, spot)], t + 179).await;

    // Window still open: the client has the spot line only (raw TCP
    // starts streaming after json_stream's 500 ms WebSocket peek), and
    // three more ticks pass without a grade.
    wait_until("the spot line", || h.spot_lines() > 0).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(h.spot_lines(), 1);
    assert!(h.grade_lines().is_empty(), "{:?}", h.grade_lines());

    h.send(&warm_up(t + 181)).await;
    let grades = h.wait_grades(1, Duration::from_secs(1)).await;
    assert_eq!(grades, vec![expected.to_string()]);
    // The spot line came first.
    let all = h.json_lines.lock().unwrap().clone();
    assert!(
        !all.iter()
            .find(|l| !l.contains("\"kind\":\"hello\""))
            .unwrap()
            .contains("\"kind\""),
        "{all:?}"
    );

    let v: serde_json::Value = serde_json::from_str(expected).unwrap();
    let series = format!(
        "manta_rbn_compare_grades_last_hour{{band=\"{}\",grade=\"{}\"}}",
        v["band"].as_str().unwrap(),
        v["grade"].as_str().unwrap()
    );
    assert_eq!(h.metric(&series), Some(1.0), "{series}");
    assert_eq!(h.metric("manta_rbn_compare_pending"), Some(0.0));

    // Read-only: the feed received the login line and nothing else.
    assert_eq!(
        h.feed.received.lock().unwrap().clone(),
        vec![format!("{STATION_CALL}\r\n")]
    );
    h.stop().await;
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn scenario_1_match_is_published_after_the_window_closes() {
    let t = 1_791_720_000;
    run_scenario(
        0,
        vec![
            report(t + 20, "K5TR", 14025.0, "W1AW", 22),
            report(t + 35, "DL8LAS", 14025.0, "W1AW", 24),
            report(t + 50, "W3LPL", 14024.9, "W1AW", 25),
        ],
        SCENARIO_1_MATCH,
    )
    .await;
}

#[tokio::test]
async fn scenario_2_none() {
    let t = 1_791_720_600;
    // This station's own report never counts.
    run_scenario(
        1,
        vec![report(t + 30, "W3XYZ-1", 7012.3, "K5XYZ", 22)],
        SCENARIO_2_NONE,
    )
    .await;
}

#[tokio::test]
async fn scenario_3_disagree() {
    let t = 1_791_721_800;
    run_scenario(
        2,
        vec![
            report(t + 15, "K5TR", 14025.0, "W1AW", 20),
            report(t + 40, "W3LPL", 14025.0, "W1AW", 20),
        ],
        SCENARIO_3_DISAGREE,
    )
    .await;
}

#[tokio::test]
async fn scenario_4_elsewhere() {
    let t = 1_791_722_700;
    run_scenario(
        3,
        vec![report(t - 60, "DL8LAS", 7030.0, "DL1ABC", 28)],
        SCENARIO_4_ELSEWHERE,
    )
    .await;
}

#[tokio::test]
async fn feed_outage_grades_unknown() {
    let (t, spot) = scenario_spot(0);
    let mut h = Harness::start(&[], t - 400).await;
    h.send(&warm_up(t - 300)).await;
    assert_eq!(h.metric("manta_rbn_compare_feed_connected"), Some(1.0));
    h.publish(t, spot).await;

    // The relay drops mid-window.
    h.set_clock(t + 30);
    h.feed.tx.send(FeedCmd::Drop).unwrap();
    let metrics = h.metrics.clone();
    wait_until("feed_connected 0", || {
        scraped(&metrics, "manta_rbn_compare_feed_connected") == Some(0.0)
    })
    .await;
    assert_eq!(
        h.metric("manta_rbn_compare_feed_reconnects_total"),
        Some(1.0)
    );

    // The feed reconnects (after its 1 s backoff) and logs in again; it is
    // up once lines flow.
    let received = h.feed.received.clone();
    wait_until("the second login", || {
        let r = received.lock().unwrap();
        r.len() == 2 && r[1].ends_with('\n')
    })
    .await;
    h.send(&report(t + 60, "K5TR", 7021.0, "N4RV", 24)).await;
    assert_eq!(h.metric("manta_rbn_compare_feed_connected"), Some(1.0));

    h.set_clock(t + 181);
    let grades = h.wait_grades(1, Duration::from_secs(1)).await;
    let v: serde_json::Value = serde_json::from_str(&grades[0]).unwrap();
    assert_eq!(v["grade"], "unknown", "{}", grades[0]);
    assert_eq!(v["rbnSkimmers"], 0);
    assert!(v["freqOffsetHz"].is_null());
    assert_eq!(
        h.metric(r#"manta_rbn_compare_grades_last_hour{band="20m",grade="unknown"}"#),
        Some(1.0)
    );
    h.stop().await;
}

/// Code review (MAN-301 validate): the feed goes quiet inside the window and
/// its idle timeout has not fired yet. The spot is held past its close
/// rather than graded `none` on the silent stretch; once the feed shows
/// life after the close, it grades.
#[tokio::test]
async fn silent_feed_holds_the_grade_until_the_feed_shows_life() {
    let (t, spot) = scenario_spot(1);
    let mut h = Harness::start(&[], t - 400).await;
    h.send(&warm_up(t - 300)).await;
    h.publish(t, spot).await;

    h.set_clock(t + 200);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(h.grade_lines().is_empty(), "{:?}", h.grade_lines());
    assert_eq!(h.metric("manta_rbn_compare_pending"), Some(1.0));

    h.send(&warm_up(t + 201)).await;
    let grades = h.wait_grades(1, Duration::from_secs(1)).await;
    let v: serde_json::Value = serde_json::from_str(&grades[0]).unwrap();
    assert_eq!(v["grade"], "none", "{}", grades[0]);
    h.stop().await;
}

#[tokio::test]
async fn co_located_spotter_mode_grades_against_k5tr_only() {
    let reports = fixture_reports();
    let spots = fixture_spots();
    let start = reports.iter().map(|r| r.time).min().unwrap() - 60;
    let end = spots.iter().map(|(t, _)| *t).max().unwrap() + 181;
    let end = end.max(reports.iter().map(|r| r.time).max().unwrap());
    let mut h = Harness::start(&["K5TR"], start).await;
    h.replay(&reports, &spots, end).await;

    let expected = fixture_lines("expected-k5tr.jsonl");
    let grades = h.wait_grades(expected.len(), WAIT).await;
    assert_eq!(grades, expected);
    h.stop().await;
}

#[tokio::test]
async fn live_records_equal_manta_grade_records() {
    let reports = fixture_reports();
    let spots = fixture_spots();
    let start = reports.iter().map(|r| r.time).min().unwrap() - 60;
    let end = spots.iter().map(|(t, _)| *t).max().unwrap() + 181;
    let end = end.max(reports.iter().map(|r| r.time).max().unwrap());
    let mut h = Harness::start(&[], start).await;
    h.replay(&reports, &spots, end).await;

    let expected = fixture_lines("expected-all.jsonl");
    let grades = h.wait_grades(expected.len(), WAIT).await;
    assert_eq!(
        grades, expected,
        "live grade lines must equal `manta grade`'s byte for byte"
    );
    // The design section's four records are this fixture's.
    assert_eq!(
        expected,
        vec![
            SCENARIO_1_MATCH,
            SCENARIO_2_NONE,
            SCENARIO_3_DISAGREE,
            SCENARIO_4_ELSEWHERE
        ]
    );
    // No more than one grade per spot.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.grade_lines().len(), expected.len());
    assert_eq!(h.spot_lines(), spots.len());
    h.stop().await;
}

#[tokio::test]
async fn shutdown_stops_the_comparator_and_feed() {
    let (t, spot) = scenario_spot(0);
    let mut h = Harness::start(&[], t - 400).await;
    h.send(&warm_up(t - 300)).await;
    h.publish(t, spot).await;
    assert_eq!(h.metric("manta_rbn_compare_pending"), Some(1.0));

    let closed = h.feed.client_closed.clone();
    let metrics = h.metrics.clone();
    let json_lines = h.json_lines.clone();
    let now = h.now.clone();
    h.stop().await;

    // The feed connection was closed by the client, and it reports down.
    wait_until("the feed connection to close", || {
        *closed.lock().unwrap() >= 1
    })
    .await;
    assert_eq!(
        scraped(&metrics, "manta_rbn_compare_feed_connected"),
        Some(0.0)
    );

    // Nothing grades after shutdown, even once the window has closed.
    now.store(t + 400, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !json_lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("spotGrade")),
        "no grade after shutdown"
    );
    assert_eq!(grades_total(&metrics), 0);
}
