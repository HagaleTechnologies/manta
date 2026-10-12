//! MAN-316 acceptance scenarios, against the real `manta` binary, a fake
//! KiwiSDR (`common::fake_kiwi_streaming`), a local list server and a
//! `TempDir` cache. No external network is needed.
//!
//!   Scenario: A week-old cty.dat is refreshed without a restart
//!   Scenario: An unreachable SCP server keeps the last good copy
//!   Scenario: A fresh install without network runs on the bundled lists
//!
//! plus file-replay determinism (a replay never downloads or reads the
//! cache) and `--cty-sha256`/`cty_sha256` pins.
//!
//! Every wait is bounded, every port is ephemeral (the `listening:` banner,
//! or a bind-then-drop for a port nobody listens on), and every age and
//! hash is computed here from `SystemTime::now()` and the bundled bytes, so
//! nothing depends on today's date or on which lists are vendored.
#![cfg(unix)]

mod common;

use common::{fake_kiwi_streaming, fixture_wav, line_channel, wait_bounded, KillOnDrop};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Startup and each expected line or metric.
const WAIT: Duration = Duration::from_secs(20);
/// A whole file replay or decode of the 300 s fixture.
const RUN_BUDGET: Duration = Duration::from_secs(180);
const DAY: u64 = 86_400;
/// Sent with every list the server serves.
const LAST_MODIFIED: &str = "Tue, 15 Sep 2026 20:52:28 GMT";
/// A made-up entity, so a download differs from the bundled copy by one
/// prefix and still passes every acceptance check.
const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn bundled_cty() -> &'static [u8] {
    manta_spot::CTY_DAT.as_bytes()
}

fn bundled_scp() -> &'static [u8] {
    manta_spot::MASTER_SCP.as_bytes()
}

/// The bundled cty.dat plus the QQ9 entity: a valid, newer download.
fn cty_with_qq9() -> Vec<u8> {
    let mut text = manta_spot::CTY_DAT.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(QQ9_ENTITY);
    text.into_bytes()
}

/// The bundled cty.dat without its `United States` entity (the header line
/// and its continuation lines): W1AW no longer resolves to a country.
fn cty_without_the_united_states() -> Vec<u8> {
    let mut out = String::new();
    let mut skipping = false;
    for line in manta_spot::CTY_DAT.split_inclusive('\n') {
        let header = !line.starts_with(char::is_whitespace);
        if header {
            skipping = line.starts_with("United States:");
        }
        if !skipping {
            out.push_str(line);
        }
    }
    assert!(
        out.len() < manta_spot::CTY_DAT.len(),
        "the bundled cty.dat has no `United States:` entity"
    );
    out.into_bytes()
}

/// A port nothing listens on: bound, read, and released.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Writes `<cache>/<list>/<sha>.<ext>` and a `state.json` naming it, with
/// `downloaded_at`/`checked_at` the given number of seconds ago. Returns the
/// state file's path.
fn seed_cache(
    cache: &Path,
    list: &str,
    ext: &str,
    bytes: &[u8],
    url: &str,
    downloaded_ago: u64,
    checked_ago: u64,
) -> PathBuf {
    let sha = sha256_hex(bytes);
    let dir = cache.join(list);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{sha}.{ext}")), bytes).unwrap();
    let now = now_secs();
    let state = serde_json::json!({
        "sha256": sha,
        "url": url,
        "downloaded_at": now - downloaded_ago,
        "checked_at": now - checked_ago,
        "last_modified": null,
    });
    let path = dir.join("state.json");
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    path
}

/// A plain-HTTP list server on an ephemeral port: `/cty.dat` and
/// `/MASTER.SCP` with `Content-Length`, `Last-Modified` and
/// `Connection: close`, 404 for anything else. Counts every connection and
/// every request per list.
struct ListServer {
    port: u16,
    connections: Arc<AtomicUsize>,
    cty_hits: Arc<AtomicUsize>,
    scp_hits: Arc<AtomicUsize>,
}

impl ListServer {
    fn start(cty: Vec<u8>, scp: Vec<u8>) -> ListServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = ListServer {
            port,
            connections: Arc::default(),
            cty_hits: Arc::default(),
            scp_hits: Arc::default(),
        };
        let (connections, cty_hits, scp_hits) = (
            server.connections.clone(),
            server.cty_hits.clone(),
            server.scp_hits.clone(),
        );
        let lists = Arc::new((cty, scp));
        // Never joined: it blocks in `accept` until the test binary exits.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                connections.fetch_add(1, Ordering::SeqCst);
                let (lists, cty_hits, scp_hits) =
                    (lists.clone(), cty_hits.clone(), scp_hits.clone());
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &lists, &cty_hits, &scp_hits);
                });
            }
        });
        server
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn serve_one(
    mut stream: TcpStream,
    lists: &(Vec<u8>, Vec<u8>),
    cty_hits: &AtomicUsize,
    scp_hits: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let path = head.split_whitespace().nth(1).unwrap_or("");
    let body = match path {
        "/cty.dat" => {
            cty_hits.fetch_add(1, Ordering::SeqCst);
            Some(&lists.0)
        }
        "/MASTER.SCP" => {
            scp_hits.fetch_add(1, Ordering::SeqCst);
            Some(&lists.1)
        }
        _ => None,
    };
    match body {
        Some(body) => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nLast-Modified: {LAST_MODIFIED}\r\n\
                 Content-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
        }
        None => stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?,
    }
    stream.flush()
}

/// A child's stderr lines, ANSI-stripped, plus every line seen so far, so
/// waits are order-insensitive and failures print the whole log.
struct Log {
    rx: Receiver<String>,
    seen: Vec<String>,
}

impl Log {
    fn new(rx: Receiver<String>) -> Log {
        Log {
            rx,
            seen: Vec::new(),
        }
    }

    /// Returns the first line (already seen or arriving within `timeout`)
    /// containing every one of `needles`.
    fn wait_for(&mut self, needles: &[&str], timeout: Duration) -> String {
        let hit = |line: &String| needles.iter().all(|n| line.contains(n));
        if let Some(line) = self.seen.iter().find(|l| hit(l)) {
            return line.clone();
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if hit(&line) {
                        return line;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "no stderr line containing {needles:?} within {timeout:?}; stderr:\n{}",
                    self.dump()
                ),
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "manta exited before a stderr line containing {needles:?}; stderr:\n{}",
                    self.dump()
                ),
            }
        }
    }

    /// Takes every line that has arrived so far.
    fn drain(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.push(line);
        }
    }

    fn dump(&self) -> String {
        self.seen.join("\n")
    }
}

fn server_toml() -> &'static str {
    "[server]\n\
     station_callsign = \"W3XYZ\"\n\
     bind_addr = \"127.0.0.1\"\n\
     metrics_bind_addr = \"127.0.0.1\"\n\
     telnet_port = 0\n\
     json_port = 0\n\
     metrics_port = 0\n\
     status_interval_secs = 0\n"
}

/// `[spot]` keys turning refresh on against `server`, with `cache`.
fn refresh_spot_toml(cache: &Path, cty_url: &str, scp_url: &str) -> String {
    format!(
        "refresh_lists = true\nlist_cache_dir = \"{}\"\ncty_url = \"{cty_url}\"\n\
         scp_url = \"{scp_url}\"\n",
        cache.display()
    )
}

/// `manta run --config <dir>/manta.toml` from a fake KiwiSDR, with a
/// `[server]` table, the given `[spot]` keys, and every listener on an
/// ephemeral loopback port.
struct Live {
    child: KillOnDrop,
    log: Log,
    metrics_port: u16,
    kiwi_done: Arc<AtomicBool>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.kiwi_done.store(true, Ordering::Relaxed);
    }
}

impl Live {
    fn start(dir: &Path, spot: &str) -> Live {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let kiwi_port = listener.local_addr().unwrap().port();
        let kiwi_done = Arc::new(AtomicBool::new(false));
        std::thread::spawn({
            let done = kiwi_done.clone();
            move || fake_kiwi_streaming(listener, done)
        });
        let config = dir.join("manta.toml");
        std::fs::write(
            &config,
            format!(
                "{}\n[spot]\n{spot}\n[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\n\
                 port = {kiwi_port}\nfreq_hz = 14025000.0\n",
                server_toml()
            ),
        )
        .unwrap();
        let mut child = KillOnDrop(
            Command::new(env!("CARGO_BIN_EXE_manta"))
                .args(["run", "--config"])
                .arg(&config)
                .env("RUST_LOG", "info")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut log = Log::new(line_channel(child.0.stderr.take().unwrap()));
        let banner = log.wait_for(&["listening:", "metrics=127.0.0.1:"], WAIT);
        let metrics_port: u16 = banner
            .split("metrics=127.0.0.1:")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no metrics port in {banner:?}"));
        log.wait_for(&["manta: listening;"], WAIT);
        Live {
            child,
            log,
            metrics_port,
            kiwi_done,
        }
    }

    fn assert_still_running(&mut self, during: &str) {
        self.log.drain();
        assert!(
            self.child.0.try_wait().unwrap().is_none(),
            "manta exited {during}; stderr:\n{}",
            self.log.dump()
        );
    }

    /// Polls `/metrics` until `ready(body)`, for at most [`WAIT`].
    fn wait_for_metrics(&mut self, what: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        let mut last = String::new();
        while Instant::now() < deadline {
            if let Some((status, body)) = http_get(self.metrics_port, "/metrics") {
                if status.contains("200") && ready(&body) {
                    return body;
                }
                last = body;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.log.drain();
        panic!(
            "/metrics never showed {what} within {WAIT:?}; last scrape:\n{last}\nstderr:\n{}",
            self.log.dump()
        );
    }

    /// Polls `/healthz` until it answers 200, for at most [`WAIT`].
    fn wait_for_healthy(&mut self) {
        let deadline = Instant::now() + WAIT;
        let mut last = None;
        while Instant::now() < deadline {
            last = http_get(self.metrics_port, "/healthz");
            if matches!(&last, Some((status, _)) if status == "HTTP/1.1 200 OK") {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("/healthz never answered 200 within {WAIT:?}; last: {last:?}");
    }
}

/// `GET <path>` over a raw TCP connection; `(status_line, body)`.
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
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
    Some((head.lines().next()?.to_string(), body.to_string()))
}

/// The value of the Prometheus sample `metric_with_labels` (the exact text
/// before the value), if present.
fn metric_value(body: &str, metric_with_labels: &str) -> Option<f64> {
    let prefix = format!("{metric_with_labels} ");
    body.lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|v| v.trim().parse().ok())
}

fn refresh_total(list: &str, outcome: &str) -> String {
    format!("manta_list_refresh_total{{list=\"{list}\",outcome=\"{outcome}\"}}")
}

fn list_info(list: &str, sha: &str, origin: &str) -> String {
    format!("manta_list_info{{list=\"{list}\",sha256=\"{sha}\",origin=\"{origin}\"}}")
}

/// Runs `cmd` to completion within [`RUN_BUDGET`], capturing both streams.
fn run_to_end(cmd: &mut Command) -> Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let Some(status) = wait_bounded(&mut child, RUN_BUDGET) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("manta did not finish within {RUN_BUDGET:?}: {cmd:?}");
    };
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn assert_success(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed ({:?}); stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn scenario_1_a_week_old_cty_is_refreshed_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let new_cty = cty_with_qq9();
    let new_sha = sha256_hex(&new_cty);
    let bundled_cty_sha = sha256_hex(bundled_cty());
    let server = ListServer::start(new_cty.clone(), bundled_scp().to_vec());
    let (cty_url, scp_url) = (server.url("/cty.dat"), server.url("/MASTER.SCP"));
    // Given a cache whose cty.dat (the bundled bytes) was checked 8 days ago.
    seed_cache(
        &cache,
        "cty",
        "dat",
        bundled_cty(),
        &cty_url,
        8 * DAY,
        8 * DAY,
    );

    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    let pid = daemon.child.0.id();
    daemon.log.wait_for(
        &[&format!(
            "lists: cty from the cache (sha256 {bundled_cty_sha}, downloaded "
        )],
        WAIT,
    );
    daemon.log.wait_for(
        &[&format!(
            "list refresh: on; cty every 7 days from {cty_url}, scp every day from \
             {scp_url}; cache {}",
            cache.display()
        )],
        WAIT,
    );

    // When the refresher finds it due, then the new version is in use.
    daemon.log.wait_for(
        &[
            &format!("list refresh: cty updated to sha256 {new_sha} ("),
            &format!(" prefixes, Last-Modified {LAST_MODIFIED}); new spots use it now"),
        ],
        WAIT,
    );
    let cty_updated = refresh_total("cty", "updated");
    let cty_info = list_info("cty", &new_sha, "download");
    let body = daemon.wait_for_metrics("the cty update and an scp attempt", |b| {
        metric_value(b, &cty_updated) == Some(1.0)
            && metric_value(b, &cty_info) == Some(1.0)
            && metric_value(b, &refresh_total("scp", "updated")).unwrap_or(0.0)
                + metric_value(b, &refresh_total("scp", "unchanged")).unwrap_or(0.0)
                >= 1.0
    });
    // SCP had no state, so it was due in the same pass; the server holds
    // the bundled copy, so it can only end updated or unchanged.
    assert_eq!(
        metric_value(&body, &refresh_total("scp", "failed")),
        Some(0.0),
        "{body}"
    );
    assert_eq!(
        metric_value(&body, &refresh_total("cty", "failed")),
        Some(0.0),
        "{body}"
    );

    // And the cache holds it, with state.json naming it.
    let version = cache.join("cty").join(format!("{new_sha}.dat"));
    assert_eq!(
        std::fs::read(&version).unwrap_or_else(|e| panic!("{}: {e}", version.display())),
        new_cty
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cache.join("cty/state.json")).unwrap()).unwrap();
    assert_eq!(state["sha256"], new_sha.as_str(), "{state}");
    assert_eq!(state["url"], cty_url.as_str(), "{state}");

    // Without a restart: the same process, still healthy.
    daemon.assert_still_running("after the cty refresh");
    assert_eq!(daemon.child.0.id(), pid);
    daemon.wait_for_healthy();
    assert_eq!(server.cty_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn scenario_2_an_unreachable_scp_server_keeps_the_last_good_copy() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let scp_sha = sha256_hex(bundled_scp());
    let server = ListServer::start(bundled_cty().to_vec(), bundled_scp().to_vec());
    let cty_url = server.url("/cty.dat");
    let scp_port = closed_port();
    let scp_url = format!("http://127.0.0.1:{scp_port}/MASTER.SCP");
    // Given a cached MASTER.SCP last checked two days ago (due: daily).
    let state_path = seed_cache(
        &cache,
        "scp",
        "scp",
        bundled_scp(),
        &scp_url,
        2 * DAY,
        2 * DAY,
    );
    let seeded_state = std::fs::read(&state_path).unwrap();

    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    daemon.log.wait_for(
        &[&format!(
            "scp from the cache (sha256 {scp_sha}, downloaded "
        )],
        WAIT,
    );

    // When the SCP server is unreachable, then the failure is logged ...
    daemon.log.wait_for(
        &[
            &format!("warning: list refresh: scp failed: connecting to 127.0.0.1:{scp_port}"),
            &format!("; still using sha256 {scp_sha} (from the cache); next try in 1 hour"),
        ],
        WAIT,
    );
    // ... counted, and the cached copy stays in use.
    let scp_failed = refresh_total("scp", "failed");
    let scp_info = list_info("scp", &scp_sha, "cache");
    let body = daemon.wait_for_metrics("one scp failure", |b| {
        metric_value(b, &scp_failed) == Some(1.0) && metric_value(b, &scp_info) == Some(1.0)
    });
    assert_eq!(
        metric_value(&body, &refresh_total("scp", "updated")),
        Some(0.0),
        "{body}"
    );
    assert_eq!(
        body.lines()
            .filter(|l| l.starts_with("manta_list_info{list=\"scp\""))
            .count(),
        1,
        "one scp series, the cached copy:\n{body}"
    );
    // A failure is not persisted: the cache's state is untouched.
    assert_eq!(std::fs::read(&state_path).unwrap(), seeded_state);
    daemon.assert_still_running("after the scp failure");
}

#[test]
fn scenario_3a_a_fresh_install_without_network_runs_on_the_bundled_lists() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let (cty_sha, scp_sha) = (sha256_hex(bundled_cty()), sha256_hex(bundled_scp()));
    let (cty_port, scp_port) = (closed_port(), closed_port());
    let cty_url = format!("http://127.0.0.1:{cty_port}/cty.dat");
    let scp_url = format!("http://127.0.0.1:{scp_port}/MASTER.SCP");

    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    daemon.log.wait_for(
        &[&format!(
            "lists: cty built in (sha256 {cty_sha}); scp built in (sha256 {scp_sha})"
        )],
        WAIT,
    );
    daemon.log.wait_for(&["ready: decoding"], WAIT);
    for (list, port, sha) in [("cty", cty_port, &cty_sha), ("scp", scp_port, &scp_sha)] {
        daemon.log.wait_for(
            &[
                &format!("warning: list refresh: {list} failed: connecting to 127.0.0.1:{port}"),
                &format!("; still using sha256 {sha} (built in); next try in 1 hour"),
            ],
            WAIT,
        );
    }
    let body = daemon.wait_for_metrics("two failures on the bundled lists", |b| {
        metric_value(b, &refresh_total("cty", "failed")) == Some(1.0)
            && metric_value(b, &refresh_total("scp", "failed")) == Some(1.0)
    });
    assert_eq!(
        metric_value(&body, &list_info("cty", &cty_sha, "bundled")),
        Some(1.0),
        "{body}"
    );
    assert_eq!(
        metric_value(&body, &list_info("scp", &scp_sha, "bundled")),
        Some(1.0),
        "{body}"
    );
    daemon.assert_still_running("without a network");
}

#[test]
fn scenario_3b_with_refresh_unset_nothing_is_downloaded_or_logged() {
    let dir = tempfile::tempdir().unwrap();
    let (cty_sha, scp_sha) = (sha256_hex(bundled_cty()), sha256_hex(bundled_scp()));
    let server = ListServer::start(bundled_cty().to_vec(), bundled_scp().to_vec());
    // URLs set, refresh_lists left at its default (off).
    let spot = format!(
        "cty_url = \"{}\"\nscp_url = \"{}\"\n",
        server.url("/cty.dat"),
        server.url("/MASTER.SCP")
    );

    let mut daemon = Live::start(dir.path(), &spot);
    daemon.log.wait_for(&["ready: decoding"], WAIT);
    // Every family is rendered on each scrape, so one showing the lists in
    // use is one where the refresh families would show too.
    let cty_info = list_info("cty", &cty_sha, "bundled");
    let scp_info = list_info("scp", &scp_sha, "bundled");
    let body = daemon.wait_for_metrics("the bundled lists in use", |b| {
        metric_value(b, &cty_info) == Some(1.0) && metric_value(b, &scp_info) == Some(1.0)
    });
    // Without a cache both lists would be due at once, so a refresher that
    // had started would have connected by now; give it 2 s to.
    std::thread::sleep(Duration::from_secs(2));
    let (_, body_later) = http_get(daemon.metrics_port, "/metrics").expect("GET /metrics");
    for scrape in [&body, &body_later] {
        assert!(
            !scrape
                .lines()
                .any(|l| l.starts_with("manta_list_refresh_total")
                    || l.starts_with("manta_list_last_success_timestamp_seconds")),
            "refresh samples with refresh_lists unset:\n{scrape}"
        );
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
    daemon.assert_still_running("with refresh unset");
    assert!(
        !daemon
            .log
            .seen
            .iter()
            .any(|l| l.contains("lists: cty") || l.contains("list refresh")),
        "list lines with refresh_lists unset; stderr:\n{}",
        daemon.log.dump()
    );
}

#[test]
fn file_replay_never_downloads_or_reads_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (cty_sha, scp_sha) = (sha256_hex(bundled_cty()), sha256_hex(bundled_scp()));
    let new_cty = cty_with_qq9();
    let new_sha = sha256_hex(&new_cty);
    let server = ListServer::start(new_cty.clone(), bundled_scp().to_vec());
    let (cty_url, scp_url) = (server.url("/cty.dat"), server.url("/MASTER.SCP"));
    // A cache whose current cty.dat is newer than the bundled one, and due.
    seed_cache(&cache, "cty", "dat", &new_cty, &cty_url, DAY, 8 * DAY);
    let config = dir.path().join("manta.toml");
    std::fs::write(
        &config,
        format!("[spot]\n{}", refresh_spot_toml(&cache, &cty_url, &scp_url)),
    )
    .unwrap();

    let replay = |config: Option<&Path>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        cmd.args(["run", "--source"]).arg(fixture_wav()).args([
            "--dial-freq-hz",
            "14060000",
            "--json",
        ]);
        if let Some(config) = config {
            cmd.arg("--config").arg(config);
        }
        let out = run_to_end(&mut cmd);
        assert_success(&out, "manta run --source");
        out
    };
    let plain = replay(None);
    let with_refresh = replay(Some(&config));

    let stderr = String::from_utf8_lossy(&with_refresh.stderr);
    let note = format!(
        "note: list refresh is off for file replay, so the run is repeatable; using cty built \
         in (sha256 {cty_sha}) and scp built in (sha256 {scp_sha}); pin a downloaded copy with \
         --cty-sha256 or --scp-sha256"
    );
    assert!(stderr.contains(&note), "no {note:?} in stderr:\n{stderr}");
    assert!(
        !stderr.contains(&new_sha),
        "the replay named the cached copy:\n{stderr}"
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
    assert!(
        plain.stdout == with_refresh.stdout,
        "refresh_lists changed a file replay's output ({} vs {} bytes)",
        plain.stdout.len(),
        with_refresh.stdout.len()
    );
}

#[test]
fn decode_with_a_cache_pin_is_repeatable_and_uses_the_pinned_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    // A deliberately unacceptable download (no United States), which a
    // pin still allows.
    let pinned = cty_without_the_united_states();
    let sha = sha256_hex(&pinned);
    let version = cache.join("cty").join(format!("{sha}.dat"));
    std::fs::create_dir_all(version.parent().unwrap()).unwrap();
    std::fs::write(&version, &pinned).unwrap();
    let config = dir.path().join("manta.toml");
    std::fs::write(
        &config,
        format!("[spot]\nlist_cache_dir = \"{}\"\n", cache.display()),
    )
    .unwrap();

    let decode = |pin: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        cmd.args(["decode", "--json"])
            .arg(fixture_wav())
            .arg("--config")
            .arg(&config);
        if let Some(pin) = pin {
            cmd.args(["--cty-sha256", pin]);
        }
        let out = run_to_end(&mut cmd);
        assert_success(&out, "manta decode");
        out
    };
    let spots = |out: &Output| -> Vec<String> {
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        report["spots"]
            .as_array()
            .expect("decode --json output must include a 'spots' array")
            .iter()
            .map(|s| s["callsign"].as_str().unwrap().to_string())
            .collect()
    };

    let first = decode(Some(&sha));
    let second = decode(Some(&sha));
    assert!(
        first.stdout == second.stdout,
        "a pinned decode is not repeatable"
    );
    let stderr = String::from_utf8_lossy(&first.stderr);
    let lists = format!("lists: cty pinned (sha256 {sha}, {})", version.display());
    assert!(stderr.contains(&lists), "no {lists:?} in stderr:\n{stderr}");
    let pinned_spots = spots(&first);
    assert!(
        !pinned_spots.iter().any(|c| c == "W1AW"),
        "W1AW spotted with the United States removed: {pinned_spots:?}"
    );

    // Control: the same decode without the pin spots W1AW.
    let control = decode(None);
    let control_spots = spots(&control);
    assert!(
        control_spots.iter().any(|c| c == "W1AW"),
        "the bundled cty.dat did not spot W1AW: {control_spots:?}"
    );
}

#[test]
fn run_with_refresh_on_and_a_pin_never_refreshes_the_pinned_list() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let pinned = cty_with_qq9();
    let sha = sha256_hex(&pinned);
    let server = ListServer::start(pinned.clone(), bundled_scp().to_vec());
    let (cty_url, scp_url) = (server.url("/cty.dat"), server.url("/MASTER.SCP"));
    // Cached, and due were it not pinned.
    seed_cache(&cache, "cty", "dat", &pinned, &cty_url, 8 * DAY, 8 * DAY);
    let version = cache.join("cty").join(format!("{sha}.dat"));
    let spot = format!(
        "{}cty_sha256 = \"{sha}\"\n",
        refresh_spot_toml(&cache, &cty_url, &scp_url)
    );

    let mut daemon = Live::start(dir.path(), &spot);
    daemon.log.wait_for(
        &[&format!(
            "lists: cty pinned (sha256 {sha}, {})",
            version.display()
        )],
        WAIT,
    );
    daemon.log.wait_for(
        &[&format!(
            "list refresh: on; scp every day from {scp_url}; cache {}",
            cache.display()
        )],
        WAIT,
    );
    daemon.log.wait_for(
        &["note: cty_sha256 is set, so list refresh leaves cty.dat alone"],
        WAIT,
    );
    let body = daemon.wait_for_metrics("an scp attempt", |b| {
        metric_value(b, &refresh_total("scp", "updated")).unwrap_or(0.0)
            + metric_value(b, &refresh_total("scp", "unchanged")).unwrap_or(0.0)
            >= 1.0
    });
    assert!(server.scp_hits.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        server.cty_hits.load(Ordering::SeqCst),
        0,
        "the pinned cty.dat was downloaded"
    );
    assert!(
        !body.contains("manta_list_refresh_total{list=\"cty\""),
        "the pinned cty.dat has refresh samples:\n{body}"
    );
    daemon.assert_still_running("with a pinned cty.dat");
}
