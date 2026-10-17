//! MAN-81: `manta health` turns a daemon's `/healthz` into the 0/1 exit code
//! a container health check reads. A fake HTTP peer drives every exit-code
//! branch; a real `manta run` proves the metrics port comes from the same
//! config the daemon runs. See
//! docs/DECISIONS/2026-10-11-man81-docker-config-mount-and-healthcheck.md.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // `health` reads `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>`, so the test
    // runner's own environment must never leak into the child. Tests that
    // exercise the env tier set theirs explicitly.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A fake metrics listener on 127.0.0.1 that answers up to `conns`
/// connections with `response`: it reads each request through the blank
/// line, writes the canned reply, half-closes, and waits for the client to
/// close. Each request's first line is sent on the returned channel.
fn fake_server(response: Vec<u8>, conns: usize) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..conns {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                }
            }
            let _ = tx.send(first);
            let _ = stream.write_all(&response);
            // MAN-156: close gracefully. Dropping the socket straight after the
            // write let Windows reset the connection before `manta health` read
            // the body ("forcibly closed", os error 10054; about 1 in 8 Windows
            // merge-queue runs). Send FIN, then wait, bounded, for the client
            // to close its side.
            let _ = stream.shutdown(Shutdown::Write);
            let _ = reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_secs(5)));
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        }
    });
    (addr, rx)
}

fn http_reply(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const HEALTHY_BODY: &str = "ok\nsource file: healthy\ndecode: ok\n";
const UNHEALTHY_BODY: &str = "unhealthy\nsource kiwi: unhealthy\ndecode: stalled 12s\n";

fn health_at(addr: SocketAddr) -> Output {
    manta()
        .args(["health", "--addr", &addr.to_string()])
        .output()
        .unwrap()
}

fn write_config(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn health_reports_a_healthy_daemon_and_exits_zero() {
    let (addr, requests) = fake_server(http_reply("200 OK", HEALTHY_BODY), 1);
    let o = health_at(addr);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    assert_eq!(stdout(&o), HEALTHY_BODY);
    // A probe that runs every 10 s must not log, warn or nag on stderr.
    assert_eq!(stderr(&o), "");
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("GET /healthz HTTP/1.1"),
        "request line: {request:?}"
    );
}

#[test]
fn health_prints_an_unhealthy_report_and_exits_one() {
    let (addr, _) = fake_server(http_reply("503 Service Unavailable", UNHEALTHY_BODY), 1);
    let o = health_at(addr);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert_eq!(stdout(&o), UNHEALTHY_BODY);
}

#[test]
fn health_escapes_peer_text() {
    let body = "unhealthy\nsource \x1b[31mkiwi\x1b[0m: unhealthy\n";
    let (addr, _) = fake_server(http_reply("503 Service Unavailable", body), 1);
    let o = health_at(addr);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(
        !o.stdout.contains(&0x1b),
        "raw ESC on stdout: {:?}",
        stdout(&o)
    );
    assert!(stdout(&o).starts_with("unhealthy\n"), "{:?}", stdout(&o));
}

#[test]
fn health_rejects_a_reply_that_is_not_a_health_report() {
    for (status, body) in [
        ("200 OK", "hello\n"),
        ("404 Not Found", "not found\n"),
        ("200 OK", ""),
    ] {
        let (addr, _) = fake_server(http_reply(status, body), 1);
        let o = health_at(addr);
        assert_eq!(
            o.status.code(),
            Some(1),
            "{status} {body:?}: stderr {}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains("did not answer like a manta health endpoint"),
            "{status} {body:?}: {}",
            stderr(&o)
        );
        assert_eq!(stdout(&o), "", "{status} {body:?}");
    }
}

/// Bound but never accepted: the handshake completes into the backlog and
/// nothing ever answers (the pattern of cli.rs's status timeout test), so
/// the probe must give up on its own bound.
#[test]
fn health_against_a_silent_listener_times_out_with_exit_one() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let start = Instant::now();
    let o = manta()
        .args(["health", "--addr", &addr.to_string(), "--timeout-secs", "2"])
        .output()
        .unwrap();
    let took = start.elapsed();
    drop(listener);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(
        took < Duration::from_secs(5),
        "--timeout-secs 2 took {took:?}"
    );
    assert!(
        stderr(&o).contains(&addr.to_string()),
        "stderr must name {addr}: {}",
        stderr(&o)
    );
}

#[test]
fn health_against_a_closed_port_exits_one_not_two() {
    let port = free_port();
    let o = manta()
        .args(["health", "--addr", &format!("127.0.0.1:{port}")])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).starts_with("manta health: "), "{}", stderr(&o));
}

#[test]
fn health_with_a_missing_config_exits_one_and_names_the_path() {
    let missing = "/nonexistent/manta.toml";
    let by_flag = manta()
        .args(["health", "--config", missing])
        .output()
        .unwrap();
    let by_env = manta()
        .arg("health")
        .env("MANTA_CONFIG", missing)
        .output()
        .unwrap();
    for (what, o) in [("--config", by_flag), ("MANTA_CONFIG", by_env)] {
        assert_eq!(o.status.code(), Some(1), "{what}: stderr {}", stderr(&o));
        assert!(stderr(&o).contains(missing), "{what}: {}", stderr(&o));
        assert!(
            stderr(&o).starts_with("manta health: "),
            "{what}: {}",
            stderr(&o)
        );
    }
}

#[test]
fn health_with_a_config_without_a_server_table_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path(), "decode-only.toml", "[decode]\n");
    let o = manta()
        .arg("health")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("[server]"), "{}", stderr(&o));
}

#[test]
fn health_config_and_addr_are_a_usage_error() {
    let o = manta()
        .args([
            "health",
            "--config",
            "manta.toml",
            "--addr",
            "127.0.0.1:7302",
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("cannot be used with"), "{}", stderr(&o));
}

#[test]
fn health_dials_the_metrics_address_the_config_names() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = fake_server(http_reply("200 OK", HEALTHY_BODY), 4);
    let port = addr.port();
    let server = "[server]\nstation_callsign = \"W3XYZ\"\n";

    let cfg = write_config(
        dir.path(),
        "port.toml",
        &format!("{server}metrics_port = {port}\n"),
    );
    let o = manta()
        .arg("health")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "--config: {}", stderr(&o));
    assert_eq!(stdout(&o), HEALTHY_BODY);

    let o = manta()
        .arg("health")
        .env("MANTA_CONFIG", &cfg)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "MANTA_CONFIG: {}", stderr(&o));

    // The `MANTA_*` overlay `run` applies wins over the file here too.
    let wrong = write_config(
        dir.path(),
        "wrong.toml",
        &format!("{server}metrics_port = {}\n", free_port()),
    );
    let o = manta()
        .arg("health")
        .arg("--config")
        .arg(&wrong)
        .env("MANTA_SERVER_METRICS_PORT", port.to_string())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "overlay: {}", stderr(&o));

    // A wildcard bind is dialled on loopback.
    let wildcard = write_config(
        dir.path(),
        "wildcard.toml",
        &format!("{server}metrics_bind_addr = \"0.0.0.0\"\nmetrics_port = {port}\n"),
    );
    let o = manta()
        .arg("health")
        .arg("--config")
        .arg(&wildcard)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "0.0.0.0: {}", stderr(&o));
}

/// Kills and reaps the daemon on drop, so a failed assertion leaks nothing.
#[cfg(unix)]
struct ChildGuard(std::process::Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn health_tracks_a_real_daemon() {
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let metrics_port = free_port();
    let cfg = write_config(
        dir.path(),
        "manta.toml",
        &format!(
            "[server]\nstation_callsign = \"W3XYZ\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_port = {metrics_port}\n"
        ),
    );
    let mut child = manta()
        .arg("run")
        .arg("--config")
        .arg(&cfg)
        .arg("--source")
        .arg(common::fixture_wav())
        .args(["--dial-freq-hz", "14060000"])
        .env("MANTA_SERVER_TELNET_PORT", "0")
        .env("MANTA_SERVER_JSON_PORT", "0")
        .env("MANTA_SERVER_STATUS_INTERVAL_SECS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = common::line_channel(child.stderr.take().unwrap());
    let mut daemon = ChildGuard(child);
    common::expect_line(&lines, &["listening:"], Duration::from_secs(60));

    let deadline = Instant::now() + Duration::from_secs(10);
    let healthy = loop {
        let o = manta()
            .arg("health")
            .arg("--config")
            .arg(&cfg)
            .output()
            .unwrap();
        if o.status.code() == Some(0) {
            break o;
        }
        assert!(
            Instant::now() < deadline,
            "never healthy: exit {:?}, stdout {:?}, stderr {:?}",
            o.status.code(),
            stdout(&o),
            stderr(&o)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(stdout(&healthy).lines().next(), Some("ok"));

    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) },
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
    common::wait_bounded(&mut daemon.0, Duration::from_secs(15))
        .expect("SIGTERM did not stop manta within 15 s");

    let o = manta()
        .arg("health")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(
        stderr(&o).contains("could not reach daemon"),
        "{}",
        stderr(&o)
    );
}
