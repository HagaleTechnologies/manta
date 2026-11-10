//! MAN-316 acceptance scenarios, against the real `manta` binary, a fake
//! KiwiSDR (`common::fake_kiwi_streaming`), a local list server and a
//! `TempDir` cache. No external network is needed.
//!
//!   Scenario: A week-old cty.dat is refreshed without a restart
//!   Scenario: An unreachable SCP server keeps the last good copy
//!   Scenario: A fresh install without network runs on the bundled lists
//!   Scenario: An https list URL is fetched with certificate checks (MAN-536)
//!   MAN-542 Scenario: The pattern list refreshes with the others
//!   MAN-542 Scenario: No URL means no download
//!
//! plus file-replay determinism (a replay never downloads or reads the
//! cache) and `--cty-sha256`/`cty_sha256` pins.
//!
//! Every wait is bounded, every port is ephemeral (the `listening:` banner,
//! or a retained, non-listening socket for an unreachable source), and every age and
//! hash is computed here from `SystemTime::now()`, the bundled bytes and the
//! bundled retrieval dates (`manta_spot::vintage`; a seeded download always
//! postdates them), so nothing depends on today's date or on which lists
//! are vendored.
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

/// MAN-542.
fn bundled_patterns() -> &'static [u8] {
    manta_spot::PATTERNS_LST.as_bytes()
}

/// MAN-542: the bundled pattern list in RBN's Skimmer style (CRLF,
/// `+ PATTERN` or two spaces and `PATTERN`), plus `+ QQ9@@@` and two lines
/// no admitted call can match: 6,037 patterns, 2 skipped.
fn skimmer_patterns_with_qq9() -> Vec<u8> {
    let mut out: String = manta_spot::PATTERNS_LST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.strip_prefix('+') {
            Some(pattern) => format!("+ {pattern}\r\n"),
            None => format!("  {line}\r\n"),
        })
        .collect();
    out.push_str("+ QQ9@@@\r\n  3E5##@@@\r\n  A2#@\r\n");
    out.into_bytes()
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

/// Reserves an ephemeral source port until the caller drops the socket.
/// Never listen or enable reuse: connections must fail while other listeners
/// cannot acquire this address. macOS times out instead of actively refusing.
/// Keep the returned socket alive through teardown.
fn refused_source() -> (socket2::Socket, u16) {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    socket.bind(&address.into()).unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    (socket, port)
}

#[test]
fn refused_sources_reserve_distinct_ports_without_listening() {
    let (_first_socket, first_port) = refused_source();
    let (_second_socket, second_port) = refused_source();
    assert_ne!(first_port, second_port);
    // A bound, non-listening TCP socket drops SYNs on macOS; Linux sends a reset.
    // Check the exact platform error without allowing a successful handshake.
    let expected_error = if cfg!(target_os = "macos") {
        std::io::ErrorKind::TimedOut
    } else {
        std::io::ErrorKind::ConnectionRefused
    };
    let addresses =
        [first_port, second_port].map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    for address in addresses {
        assert_eq!(
            TcpStream::connect_timeout(&address, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            expected_error,
            "source {address} must remain unreachable"
        );
        assert_eq!(
            TcpListener::bind(address).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse,
            "source {address} must stay reserved"
        );
    }
    let _listeners: Vec<_> = (0..8)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    for address in addresses {
        assert_eq!(
            TcpStream::connect_timeout(&address, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            expected_error,
            "source {address} must remain unreachable alongside other listeners"
        );
    }
}

/// The vendored copy's retrieval date (`manta_spot::vintage`) for `list`.
fn vendored_retrieved_at(list: &str) -> Option<SystemTime> {
    match list {
        "cty" => manta_spot::vintage::cty_dat_retrieved_at(),
        "scp" => manta_spot::vintage::master_scp_retrieved_at(),
        "patterns" => manta_spot::vintage::patterns_lst_generated_at(),
        other => panic!("no list {other:?}"),
    }
}

/// `downloaded_ago` seconds before `now`, moved to one second after
/// `retrieved` when that is later. Startup ignores a cached copy downloaded
/// on or before the vendored copy's retrieval date (D3), so a seeded copy
/// must postdate it whichever date the vendored lists carry; only
/// `checked_at` decides when a list is due.
fn seeded_downloaded_at(retrieved: Option<SystemTime>, now: u64, downloaded_ago: u64) -> u64 {
    let retrieved = retrieved.map_or(0, |at| at.duration_since(UNIX_EPOCH).unwrap().as_secs());
    (now - downloaded_ago).max(retrieved + 1)
}

/// Writes `<cache>/<list>/<sha>.<ext>` and a `state.json` naming it, with
/// `checked_at` the given number of seconds ago and `downloaded_at` the given
/// number of seconds ago or just after the vendored copy's retrieval date,
/// whichever is later (`seeded_downloaded_at`). Returns the state file's
/// path.
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
        "downloaded_at": seeded_downloaded_at(vendored_retrieved_at(list), now, downloaded_ago),
        "checked_at": now - checked_ago,
        "last_modified": null,
    });
    let path = dir.join("state.json");
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    path
}

/// A plain-HTTP list server on an ephemeral port: `/cty.dat`,
/// `/MASTER.SCP` and (MAN-542) `/Patt3Ch.lst` with `Content-Length`,
/// `Last-Modified` and `Connection: close`, 404 for anything else. Counts
/// every connection and every request per list.
struct ListServer {
    port: u16,
    connections: Arc<AtomicUsize>,
    cty_hits: Arc<AtomicUsize>,
    scp_hits: Arc<AtomicUsize>,
    pattern_hits: Arc<AtomicUsize>,
}

/// The bodies a [`ListServer`] serves, and its per-list request counters.
struct Served {
    cty: Vec<u8>,
    scp: Vec<u8>,
    patterns: Vec<u8>,
    cty_hits: Arc<AtomicUsize>,
    scp_hits: Arc<AtomicUsize>,
    pattern_hits: Arc<AtomicUsize>,
}

impl ListServer {
    fn start(cty: Vec<u8>, scp: Vec<u8>) -> ListServer {
        ListServer::start_with_patterns(cty, scp, bundled_patterns().to_vec())
    }

    fn start_with_patterns(cty: Vec<u8>, scp: Vec<u8>, patterns: Vec<u8>) -> ListServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = ListServer {
            port,
            connections: Arc::default(),
            cty_hits: Arc::default(),
            scp_hits: Arc::default(),
            pattern_hits: Arc::default(),
        };
        let connections = server.connections.clone();
        let served = Arc::new(Served {
            cty,
            scp,
            patterns,
            cty_hits: server.cty_hits.clone(),
            scp_hits: server.scp_hits.clone(),
            pattern_hits: server.pattern_hits.clone(),
        });
        // Never joined: it blocks in `accept` until the test binary exits.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                connections.fetch_add(1, Ordering::SeqCst);
                let served = served.clone();
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &served);
                });
            }
        });
        server
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn serve_one(mut stream: TcpStream, served: &Served) -> std::io::Result<()> {
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
            served.cty_hits.fetch_add(1, Ordering::SeqCst);
            Some(&served.cty)
        }
        "/MASTER.SCP" => {
            served.scp_hits.fetch_add(1, Ordering::SeqCst);
            Some(&served.scp)
        }
        "/Patt3Ch.lst" => {
            served.pattern_hits.fetch_add(1, Ordering::SeqCst);
            Some(&served.patterns)
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

/// MAN-536: a TLS server on an ephemeral port presenting the `list_tls`
/// test certificate, which no root built into manta trusts. Counts the
/// connections (handshake attempts) and the HTTP requests read over TLS.
struct TlsListServer {
    port: u16,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

impl TlsListServer {
    fn start() -> TlsListServer {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/list_tls");
        let cert = std::fs::read(fixtures.join("localhost.der")).unwrap();
        let key = std::fs::read(fixtures.join("localhost.key.der")).unwrap();
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = TlsListServer {
            port: listener.local_addr().unwrap().port(),
            connections: Arc::default(),
            requests: Arc::default(),
        };
        let (connections, requests) = (server.connections.clone(), server.requests.clone());
        // Never joined: it blocks in `accept` until the test binary exits.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                connections.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok(conn) = rustls::ServerConnection::new(config.clone()) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, stream);
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    match tls.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                if head.ends_with(b"\r\n\r\n") {
                    requests.fetch_add(1, Ordering::SeqCst);
                    let _ = tls.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
            }
        });
        server
    }

    fn url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.port)
    }
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

/// Scenarios 1 and 2 need startup to take the seeded cache, which it does
/// only for a download after the vendored copy's retrieval date. A list
/// vendored today, or after the requested age, still loses to the seed; an
/// older one leaves the requested age alone.
#[test]
fn a_seeded_download_postdates_the_vendored_copy_whatever_its_date() {
    let now: u64 = 1_791_763_200 + 3_600; // 2026-10-12 01:00 UTC
    let midnight = now - now % DAY;
    let at = |secs: u64| Some(UNIX_EPOCH + Duration::from_secs(secs));
    for (retrieved, ago) in [(midnight, 8 * DAY), (now - DAY, 2 * DAY), (now, DAY)] {
        let downloaded = seeded_downloaded_at(at(retrieved), now, ago);
        assert!(
            downloaded > retrieved,
            "{downloaded} does not postdate {retrieved}"
        );
    }
    assert_eq!(
        seeded_downloaded_at(at(now - 100 * DAY), now, 8 * DAY),
        now - 8 * DAY
    );
    assert_eq!(seeded_downloaded_at(None, now, 2 * DAY), now - 2 * DAY);
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
        &[
            &format!("lists: cty from the cache (sha256 {bundled_cty_sha}, downloaded "),
            &format!(
                "; patterns built in (sha256 {})",
                sha256_hex(bundled_patterns())
            ),
        ],
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
    let (_scp_socket, scp_port) = refused_source();
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
    let patterns_sha = sha256_hex(bundled_patterns());
    let (_cty_socket, cty_port) = refused_source();
    let (_scp_socket, scp_port) = refused_source();
    let cty_url = format!("http://127.0.0.1:{cty_port}/cty.dat");
    let scp_url = format!("http://127.0.0.1:{scp_port}/MASTER.SCP");

    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    daemon.log.wait_for(
        &[&format!(
            "lists: cty built in (sha256 {cty_sha}); scp built in (sha256 {scp_sha}); \
             patterns built in (sha256 {patterns_sha})"
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
    // MAN-542: every daemon names its pattern list; without `pattern_url`
    // it has no refresh series.
    assert_eq!(
        metric_value(&body, &list_info("patterns", &patterns_sha, "bundled")),
        Some(1.0),
        "{body}"
    );
    assert!(
        !body.contains("manta_list_refresh_total{list=\"patterns\""),
        "{body}"
    );
    daemon.assert_still_running("without a network");
}

#[test]
fn scenario_4_an_https_list_url_is_fetched_with_certificate_checks() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let scp_sha = sha256_hex(bundled_scp());
    let server = ListServer::start(bundled_cty().to_vec(), bundled_scp().to_vec());
    let cty_url = server.url("/cty.dat");
    let tls = TlsListServer::start();
    let scp_url = tls.url("/MASTER.SCP");

    // Given an https scp_url, `run` accepts it ...
    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    daemon
        .log
        .wait_for(&[&format!("scp every day from {scp_url}")], WAIT);
    // ... connects over TLS, and refuses a certificate no built-in root
    // vouches for, before sending any request.
    daemon.log.wait_for(
        &[
            &format!(
                "warning: list refresh: scp failed: connecting to 127.0.0.1:{}: invalid peer \
                 certificate: UnknownIssuer",
                tls.port
            ),
            &format!("; still using sha256 {scp_sha} (built in); next try in 1 hour"),
        ],
        WAIT,
    );
    daemon.wait_for_metrics("one scp failure", |b| {
        metric_value(b, &refresh_total("scp", "failed")) == Some(1.0)
    });
    assert!(tls.connections.load(Ordering::SeqCst) >= 1);
    assert_eq!(tls.requests.load(Ordering::SeqCst), 0);
    daemon.assert_still_running("after the certificate failure");
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
    let patterns_sha = sha256_hex(bundled_patterns());
    let note = format!(
        "note: list refresh is off for file replay, so the run is repeatable; using cty built \
         in (sha256 {cty_sha}), scp built in (sha256 {scp_sha}) and patterns built in (sha256 \
         {patterns_sha}); pin a downloaded copy with --cty-sha256 or --scp-sha256"
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
            .map(|s| s["dxCall"].as_str().unwrap().to_string())
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

/// MAN-542 Scenario: The pattern list refreshes with the others.
#[test]
fn pattern_scenario_1_the_pattern_list_refreshes_with_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let patterns = skimmer_patterns_with_qq9();
    let sha = sha256_hex(&patterns);
    let bundled_sha = sha256_hex(bundled_patterns());
    let server = ListServer::start_with_patterns(
        bundled_cty().to_vec(),
        bundled_scp().to_vec(),
        patterns.clone(),
    );
    let (cty_url, scp_url, pattern_url) = (
        server.url("/cty.dat"),
        server.url("/MASTER.SCP"),
        server.url("/Patt3Ch.lst"),
    );
    // Given refresh on, `pattern_url` set and an empty cache.
    let spot = format!(
        "{}pattern_url = \"{pattern_url}\"\n",
        refresh_spot_toml(&cache, &cty_url, &scp_url)
    );

    let mut daemon = Live::start(dir.path(), &spot);
    daemon.log.wait_for(
        &[&format!("; patterns built in (sha256 {bundled_sha})")],
        WAIT,
    );
    daemon.log.wait_for(
        &[&format!(
            "list refresh: on; cty every 7 days from {cty_url}, scp every day from {scp_url}, \
             patterns every 7 days from {pattern_url}; cache {}",
            cache.display()
        )],
        WAIT,
    );
    // When the refresh runs, then the pattern list is downloaded and swapped in.
    daemon.log.wait_for(
        &[&format!(
            "list refresh: patterns updated to sha256 {sha} (6037 patterns, 2 unmatchable lines \
             skipped, Last-Modified {LAST_MODIFIED}); new spots use it now"
        )],
        WAIT,
    );
    // And `manta_list_refresh_total{list="patterns"}` counts the outcome.
    let updated = refresh_total("patterns", "updated");
    let info = list_info("patterns", &sha, "download");
    let body = daemon.wait_for_metrics("the pattern update", |b| {
        metric_value(b, &updated) == Some(1.0) && metric_value(b, &info) == Some(1.0)
    });
    assert_eq!(
        metric_value(&body, &refresh_total("patterns", "failed")),
        Some(0.0),
        "{body}"
    );
    assert!(
        body.lines()
            .any(|l| l.starts_with("manta_list_last_success_timestamp_seconds{list=\"patterns\"}")),
        "{body}"
    );
    let version = cache.join("patterns").join(format!("{sha}.lst"));
    assert_eq!(
        std::fs::read(&version).unwrap_or_else(|e| panic!("{}: {e}", version.display())),
        patterns
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cache.join("patterns/state.json")).unwrap()).unwrap();
    assert_eq!(state["sha256"], sha.as_str(), "{state}");
    assert_eq!(state["url"], pattern_url.as_str(), "{state}");
    assert_eq!(server.pattern_hits.load(Ordering::SeqCst), 1);
    daemon.assert_still_running("after the pattern refresh");
    drop(daemon);

    // A second daemon on the same cache starts from the download and does
    // not fetch it again: the list is not due for 7 days.
    let mut second = Live::start(dir.path(), &spot);
    second.log.wait_for(
        &[&format!(
            "; patterns from the cache (sha256 {sha}, downloaded "
        )],
        WAIT,
    );
    let info = list_info("patterns", &sha, "cache");
    second.wait_for_metrics("the cached pattern list", |b| {
        metric_value(b, &info) == Some(1.0)
    });
    second.wait_for_healthy();
    assert_eq!(server.pattern_hits.load(Ordering::SeqCst), 1);
    second.assert_still_running("on the cached pattern list");
}

/// MAN-542 Scenario: No URL means no download.
#[test]
fn pattern_scenario_2_no_url_means_no_download() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let bundled_sha = sha256_hex(bundled_patterns());
    let server = ListServer::start_with_patterns(
        bundled_cty().to_vec(),
        bundled_scp().to_vec(),
        skimmer_patterns_with_qq9(),
    );
    let (cty_url, scp_url) = (server.url("/cty.dat"), server.url("/MASTER.SCP"));
    // A cache holding a pattern version from a past run that had a URL.
    let state_path = seed_cache(
        &cache,
        "patterns",
        "lst",
        &skimmer_patterns_with_qq9(),
        &server.url("/Patt3Ch.lst"),
        60,
        60,
    );
    let seeded_state = std::fs::read(&state_path).unwrap();

    let mut daemon = Live::start(dir.path(), &refresh_spot_toml(&cache, &cty_url, &scp_url));
    daemon.log.wait_for(
        &[&format!("; patterns built in (sha256 {bundled_sha})")],
        WAIT,
    );
    let on = daemon.log.wait_for(&["list refresh: on;"], WAIT);
    assert!(!on.contains("patterns"), "{on}");
    // A full refresher pass: cty and scp both had no state, so both were due.
    let body = daemon.wait_for_metrics("a cty and an scp attempt", |b| {
        ["cty", "scp"].iter().all(|list| {
            ["updated", "unchanged", "failed"]
                .iter()
                .map(|o| metric_value(b, &refresh_total(list, o)).unwrap_or(0.0))
                .sum::<f64>()
                >= 1.0
        })
    });
    assert_eq!(server.pattern_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        server.cty_hits.load(Ordering::SeqCst) + server.scp_hits.load(Ordering::SeqCst)
    );
    assert_eq!(
        metric_value(&body, &list_info("patterns", &bundled_sha, "bundled")),
        Some(1.0),
        "{body}"
    );
    assert!(
        !body.lines().any(
            |l| l.starts_with("manta_list_refresh_total{list=\"patterns\"")
                || l.starts_with("manta_list_last_success_timestamp_seconds{list=\"patterns\"")
        ),
        "{body}"
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), seeded_state);
    daemon.assert_still_running("with no pattern_url");
}
