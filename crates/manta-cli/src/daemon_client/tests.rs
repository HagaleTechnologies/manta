use super::*;
use crate::test_support::*;

fn cfg_with(metrics_bind_addr: &str, metrics_port: u16) -> manta_server::config::ServerConfig {
    manta_server::config::ServerConfig {
        metrics_bind_addr: metrics_bind_addr.to_string(),
        metrics_port,
        ..test_server_config("W3XYZ")
    }
}

/// MAN-132 made the metrics listener bind its own `metrics_bind_addr`
/// (loopback by default) instead of `bind_addr`, so `manta status`
/// must dial that address: a public `bind_addr` says nothing about
/// where `/status` listens.
#[test]
fn status_address_follows_metrics_bind_addr_not_bind_addr() {
    let server = manta_server::config::ServerConfig {
        bind_addr: "10.0.0.5".to_string(),
        metrics_port: 17302,
        ..test_server_config("W3XYZ")
    };
    assert_eq!(server.metrics_bind_addr, "127.0.0.1", "MAN-132 default");
    assert_eq!(
        resolve_status_addr(None, Some(&server)).unwrap(),
        vec!["127.0.0.1:17302".parse().unwrap()]
    );
}

#[test]
fn status_address_prefers_explicit_addr_then_config_then_default() {
    assert_eq!(
        resolve_status_addr(Some("1.2.3.4:9999"), None).unwrap(),
        vec!["1.2.3.4:9999".parse().unwrap()]
    );
    // bind_addr 0.0.0.0 in config means "listening everywhere"; the
    // CLI still has to DIAL something, and loopback is the only
    // address guaranteed to reach the local daemon.
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("0.0.0.0", 17302))).unwrap(),
        vec!["127.0.0.1:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("::", 17302))).unwrap(),
        vec!["[::1]:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("10.0.0.5", 17302))).unwrap(),
        vec!["10.0.0.5:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, None).unwrap(),
        vec!["127.0.0.1:7302".parse().unwrap()]
    );
}

/// MAN-44 remediate regression: `--addr` must accept a hostname too,
/// the same way the `--server-config`/`bind_addr` path already does
/// (`status_address_resolves_a_hostname_bind_addr_like_the_daemon_does`
/// below) -- an explicit `--addr localhost:PORT` was being rejected
/// outright by a literal `SocketAddr` parse, contradicting
/// `docs/RUNBOOKS/uplink-health.md`'s documented cross-host invocation.
#[test]
fn status_address_resolves_a_hostname_passed_via_addr() {
    let addrs = resolve_status_addr(Some("localhost:17302"), None).unwrap();
    assert!(!addrs.is_empty(), "expected at least one resolved address");
    for addr in &addrs {
        assert!(
            addr.ip().is_loopback(),
            "expected localhost to resolve to a loopback address, got {addr}"
        );
        assert_eq!(addr.port(), 17302);
    }
}

/// MAN-44 CR-B regression: the daemon binds `bind_addr` via
/// `TcpListener::bind((host, port))`, which resolves a hostname (not
/// just a literal IP) through `ToSocketAddrs` -- so `bind_addr =
/// "localhost"` is a config the daemon runs on happily. `manta status
/// --server-config` must resolve it the same way instead of rejecting
/// a config the daemon itself accepts.
#[test]
fn status_address_resolves_a_hostname_bind_addr_like_the_daemon_does() {
    let addrs = resolve_status_addr(None, Some(&cfg_with("localhost", 17302))).unwrap();
    assert!(!addrs.is_empty(), "expected at least one resolved address");
    for addr in &addrs {
        assert!(
            addr.ip().is_loopback(),
            "expected localhost to resolve to a loopback address, got {addr}"
        );
        assert_eq!(addr.port(), 17302);
    }
}

/// Reads the client's request through its blank line before a fake
/// `/status` server answers it, as the real `metrics_http::read_headers`
/// does (without that function's line-count and line-length bounds --
/// the peer here is always this file's own `fetch_status`). A fake
/// that closes with the request still unread makes the kernel send RST
/// instead of FIN: macOS then fails the client's pending read with
/// "Connection reset by peer (os error 54)" and drops the response it
/// had not consumed yet, while Linux delivers the response first and
/// hides the reset -- why these tests were green on ubuntu-latest and
/// red on macos-latest.
async fn read_request_head(socket: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
}

/// Code-review regression (finding 1): a hostname that resolves to
/// several addresses -- e.g. `localhost` returning `::1` before
/// `127.0.0.1` on a dual-stack host -- must not make `manta status`
/// give up after dialing only the FIRST candidate. The previous
/// `resolve_status_addr`/`fetch_status_inner` kept only
/// `to_socket_addrs().next()`, so a real daemon bound IPv4-only (the
/// project's own default `bind_addr = "0.0.0.0"`) was reported as
/// unreachable whenever the resolver listed an unreachable address
/// first. This reproduces that shape directly -- a dead IPv6 loopback
/// candidate followed by a live IPv4-only listener -- so it fails on
/// any implementation that dials only the first address, regardless
/// of what a given machine's real resolver happens to return for
/// "localhost".
#[tokio::test]
async fn fetch_status_falls_back_past_an_unreachable_first_address() {
    // A closed port on ::1: nothing is listening, so connecting here
    // fails immediately (connection refused) rather than hanging.
    let dead = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 1);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let live = listener.local_addr().unwrap();
    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        let _ = socket.shutdown().await;
    });

    let doc = fetch_status(&[dead, live], std::time::Duration::from_secs(5))
        .await
        .expect("must fall back to the second address after the first refuses")
        .doc;
    assert_eq!(doc.schema_version, 1);
}

/// MAN-44 code review CR-1 regression: a black-holed first candidate
/// (SYNs silently dropped, not refused) must not consume the WHOLE
/// overall timeout budget before a later, live candidate is ever
/// tried -- same shape and same reasoning as
/// `uplink::connect_first_reachable_bounded_stops_at_the_overall_deadline`:
/// a real SYN black hole depends on undocumented host/network behavior
/// (a host with no route to a given block gets an immediate
/// `NetworkUnreachable` instead of a hang), so this fakes an
/// unconditionally hanging first attempt under paused tokio time
/// instead of dialing a real address. `connect_any`'s previous bare
/// `TcpStream::connect` with no per-candidate bound would have let
/// address 1 alone eat the entire `overall_timeout` here, never
/// reaching address 2.
#[tokio::test(start_paused = true)]
async fn connect_any_bounded_falls_through_a_stalled_first_candidate() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let overall_timeout = std::time::Duration::from_secs(10);
    let addrs: Vec<std::net::SocketAddr> = vec![
        "127.0.0.1:1".parse().unwrap(),
        "127.0.0.1:2".parse().unwrap(),
    ]; // never actually dialed -- `connect` below is faked

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_connect = attempts.clone();
    let connect = move |addr: std::net::SocketAddr| {
        let attempts = attempts_for_connect.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            if addr.port() == 1 {
                std::future::pending::<std::io::Result<()>>().await
            } else {
                Ok(())
            }
        }
    };

    let started = tokio::time::Instant::now();
    let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
        .await
        .expect("must fall through to the live second candidate");
    assert_eq!(addr.port(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() < overall_timeout,
        "the stalled first candidate must not consume the whole overall budget: elapsed {:?}",
        started.elapsed()
    );
}

/// Codex review, PR #95: a candidate that fails fast must hand its
/// unused slice of the budget to the candidates after it. Splitting
/// `overall_timeout` evenly up front gave the live fourth address here
/// only 5s / 4 = 1.25s, so a daemon that takes 3s to accept timed out
/// (exit 2, "could not reach") with nearly the whole deadline unspent.
#[tokio::test(start_paused = true)]
async fn connect_any_bounded_carries_unused_time_forward_to_later_candidates() {
    let overall_timeout = std::time::Duration::from_secs(5);
    let addrs: Vec<std::net::SocketAddr> = (1..=4)
        .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
        .collect(); // never actually dialed -- `connect` below is faked
    let connect = |addr: std::net::SocketAddr| async move {
        if addr.port() < 4 {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            ))
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            Ok(())
        }
    };

    let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
        .await
        .expect("three instant refusals must leave the fourth candidate the rest of the deadline");
    assert_eq!(addr.port(), 4);
}

/// Same terminal-injection concern as `status::render_human`'s label
/// escaping (Codex review, PR #95), on the failure path: a spoofed
/// endpoint's status line is quoted in the error `manta status` prints
/// to stderr.
#[tokio::test]
async fn fetch_status_escapes_control_characters_in_a_peer_status_line() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 500 \x1b]0;pwned\x07Oops\r\n\r\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });

    let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect_err("a 500 status line must be an error");
    let message = status_failure_message(&err);
    assert!(
        !message.chars().any(char::is_control),
        "no peer-supplied control character may reach the terminal: {message:?}"
    );
    assert!(message.contains(r"\u{1b}]0;pwned\u{7}Oops"));
}

/// serde_json's data errors quote the offending value ("unknown variant
/// `...`"), and that value comes from the peer -- same escaping as the
/// status line above (Codex review, PR #95).
#[test]
fn parse_status_doc_escapes_control_characters_quoted_in_a_serde_error() {
    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled)
        .to_json()
        .replace(r#""health": "disabled""#, r#""health": "\u001b[2J\npwned""#);
    let err = parse_status_doc(&body).expect_err("an unknown health variant must not parse");
    let message = status_failure_message(&err);
    assert!(
        message.contains("not a valid status document"),
        "{message:?}"
    );
    assert!(
        !message.chars().any(char::is_control),
        "no peer-supplied control character may reach the terminal: {message:?}"
    );
    assert!(message.contains(r"\u{1b}[2J\npwned"), "{message:?}");
}

/// Local snippets keep their source and caret, with visible line breaks.
#[test]
fn status_config_parse_errors_keep_their_escaped_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[server]\nbind_addr = \n").unwrap();
    let err = run_status(Some(&path), None, 5).expect_err("a broken config must not parse");
    let message = status_failure_message(&err);
    assert!(message.contains("TOML parse error"), "{message:?}");
    assert!(message.contains("bind_addr = ") && message.contains('^'));
    assert!(
        !message.chars().any(char::is_control) && message.contains(r"\n"),
        "the TOML snippet must keep visible line breaks: {message:?}"
    );
}

fn doc_with(
    health: manta_server::metrics::OverallUplinkHealth,
) -> manta_server::status_doc::StatusDoc {
    manta_server::status_doc::StatusDoc {
        schema_version: 1,
        version: "test".to_string(),
        uptime_seconds: 0,
        spots_total: 0,
        telnet_clients: 0,
        json_clients: 0,
        ws_clients: 0,
        active_tracks: None,
        uplink: manta_server::status_doc::UplinkStatus {
            health,
            connected_targets: 0,
            enabled_targets: 0,
            sent_total: 0,
            suppressed_total: 0,
            reconnects_total: 0,
            reconnect_window_seconds: 300,
            flapping_threshold: 3,
            targets: vec![],
        },
        inputs: vec![],
        sources: Vec::new(),
        rbn_compare: None,
    }
}

#[test]
fn parse_status_doc_keeps_fields_this_build_does_not_declare() {
    use manta_server::metrics::OverallUplinkHealth;
    let doc = doc_with(OverallUplinkHealth::Down);
    let mut value = serde_json::to_value(&doc).unwrap();
    value["foo"] = serde_json::json!(1);
    value["uplink"]["uplink_extra"] = serde_json::json!("x");

    let reply = parse_status_doc(&value.to_string()).unwrap();
    assert_eq!(reply.raw["foo"], 1);
    assert_eq!(reply.raw["uplink"]["uplink_extra"], "x");
    assert_eq!(reply.doc, doc);
    assert_eq!(status_exit_code(&reply.doc), 1);
}

#[test]
fn status_exit_code_is_zero_when_healthy_one_when_degraded() {
    use manta_server::metrics::OverallUplinkHealth;
    assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Ok)), 0);
    assert_eq!(
        status_exit_code(&doc_with(OverallUplinkHealth::Disabled)),
        0
    );
    assert_eq!(
        status_exit_code(&doc_with(OverallUplinkHealth::Degraded)),
        1
    );
    assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Down)), 1);
}

#[tokio::test]
async fn fetch_status_parses_a_chunk_split_http_response() {
    // A body assembled from one read() would pass trivially and hide
    // a real framing bug -- the status line, headers, and body are
    // deliberately written in three separate writes here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        socket
                .write_all(
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        socket.write_all(body.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    });

    let doc = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect("must parse a response split across several writes")
        .doc;
    assert_eq!(doc.schema_version, 1);
}

#[tokio::test]
async fn fetch_status_errors_cleanly_on_a_404_and_on_a_non_json_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });
    let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect_err("a 404 must be a clean error, not a panic");
    assert!(format!("{err:#}").contains("404"));

    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener2.accept().await.unwrap();
        read_request_head(&mut socket).await;
        let body = "not json";
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });
    let err = fetch_status(&[addr2], std::time::Duration::from_secs(5))
        .await
        .expect_err("a non-JSON body must be a clean error, not a panic");
    assert!(format!("{err:#}").contains("status document"));
}

#[tokio::test]
async fn fetch_status_times_out_instead_of_hanging_on_a_silent_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _peer) = listener.accept().await.unwrap();
        // Accepts, then never writes anything -- the socket stays open.
        std::future::pending::<()>().await
    });

    let started = std::time::Instant::now();
    let err = fetch_status(&[addr], std::time::Duration::from_millis(200))
        .await
        .expect_err("a silent server must time out, not hang forever");
    assert!(err.to_string().contains("timed out"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "must not have hung past the configured timeout"
    );
}
