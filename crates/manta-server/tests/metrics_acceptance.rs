//! MAN-12 acceptance scenario 3:
//!   Given manta is running as a daemon
//!   When an operator queries its metrics endpoint
//!   Then current spot rate, active track count, and per-source health are visible
//!
//! Also covers MAN-64 (PR #76 review round 7): the per-source-IP budget on
//! COMPLETED requests, distinct from MAN-61's per-IP CONNECTION quota above
//! -- see `completed_requests_from_one_ip_are_rate_limited`.

use manta_server::metrics::{InputHealth, Metrics};
use manta_server::rate_limit::IpRateLimiter;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// MAN-64: the built-in per-IP request budget, for tests that only need a
/// working listener and never approach it.
fn default_request_limiter() -> IpRateLimiter {
    IpRateLimiter::new(
        manta_server::metrics_http::MAX_METRICS_REQUESTS_PER_IP,
        manta_server::metrics_http::METRICS_REQUEST_RATE_WINDOW,
    )
}

fn sample_spot() -> Spot {
    Spot {
        callsign: "JA1ABC".to_string(),
        freq_hz: 14_027_100.0,
        snr_db: 23.0,
        wpm: 28.0,
        spot_type: SpotType::Cq,
        confidence: 0.9,
        track_id: 1,
        sample_ts: 0,
    }
}

/// Connects, sends a well-formed `GET /metrics` request, and returns the
/// response's status line. Shared by every test below that only cares
/// whether a request was admitted or refused, not the body content.
async fn get_metrics_status(addr: std::net::SocketAddr) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut status_line))
        .await
        .unwrap()
        .unwrap();
    status_line.trim_end().to_string()
}

#[tokio::test]
async fn operator_get_request_sees_spot_count_active_tracks_and_source_health() {
    let metrics = Arc::new(Metrics::new());
    metrics.record_spot(&sample_spot());
    metrics.record_spot(&sample_spot());
    metrics.record_spot(&sample_spot());
    metrics.set_active_tracks(7);
    metrics.set_source_health("soapy0", true);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            limiter,
            manta_server::tasks::IpQuota::new(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
            ),
            IpRateLimiter::new(
                manta_server::metrics_http::MAX_METRICS_REQUESTS_PER_IP,
                manta_server::metrics_http::METRICS_REQUEST_RATE_WINDOW,
            ),
        )
        .await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut status_line))
        .await
        .unwrap()
        .unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "status: {status_line:?}"
    );

    // Skip headers.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }

    let mut body = String::new();
    reader.read_to_string(&mut body).await.unwrap();

    assert!(body.contains("manta_spots_total 3"), "body: {body}");
    assert!(body.contains("manta_active_tracks 7"), "body: {body}");
    assert!(
        body.contains(r#"manta_source_health{source="soapy0"} 1"#),
        "body: {body}"
    );
}

/// MAN-64 (PR #76 review round 7): `ConnectionLimiter`/`IpQuota` bound
/// SIMULTANEOUS connections and `HEADER_READ_TIMEOUT` bounds INCOMPLETE
/// ones -- neither bounds how many complete requests one source can drive
/// through this listener over time, since the permit is released the
/// instant each response closes. Three sequential, individually
/// well-formed, individually fast requests from one IP against a
/// two-per-window budget: the first two are served, the third is refused.
#[tokio::test]
async fn completed_requests_from_one_ip_are_rate_limited() {
    let metrics = Arc::new(Metrics::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            manta_server::tasks::new_connection_limiter(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
            ),
            manta_server::tasks::IpQuota::new(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
            ),
            IpRateLimiter::new(2, Duration::from_secs(60)),
        )
        .await;
    });

    for expected in ["HTTP/1.1 200", "HTTP/1.1 200", "HTTP/1.1 429"] {
        let status = get_metrics_status(addr).await;
        assert!(
            status.starts_with(expected),
            "expected {expected}, got {status:?}"
        );
    }
}

/// The refusal must be a complete, closable HTTP response an operator's
/// scraper can act on -- not a bare socket close, which reads as a network
/// fault rather than "you are over budget." A budget of 0-per-window (via
/// `IpRateLimiter::new(0, ..)`) refuses every request from the very first
/// one, keeping this test independent of the prior test's request count.
#[tokio::test]
async fn rate_limited_response_is_well_formed_with_retry_after() {
    let metrics = Arc::new(Metrics::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            manta_server::tasks::new_connection_limiter(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
            ),
            manta_server::tasks::IpQuota::new(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
            ),
            IpRateLimiter::new(0, Duration::from_secs(60)),
        )
        .await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();

    assert!(
        response.starts_with("HTTP/1.1 429 Too Many Requests\r\n"),
        "response: {response:?}"
    );
    assert!(
        response.contains("Retry-After: 60\r\n"),
        "response: {response:?}"
    );
    assert!(
        response.contains("Content-Length: 0\r\n"),
        "response: {response:?}"
    );
    assert!(
        response.contains("Connection: close\r\n"),
        "response: {response:?}"
    );
}

/// MAN-56 Gherkin: "those counts are visible on the Prometheus /metrics
/// endpoint without needing to attach a debugger or read gap_stats()".
#[tokio::test]
async fn operator_get_request_sees_input_packet_loss_counters() {
    let metrics = Arc::new(Metrics::new());
    metrics.set_input_health(
        "hpsdr",
        InputHealth {
            dropped_packets: 9,
            gaps_detected: 2,
            malformed_packets: 4,
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
    );
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            limiter,
            manta_server::tasks::IpQuota::new(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
            ),
            default_request_limiter(),
        )
        .await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut status_line))
        .await
        .unwrap()
        .unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "status: {status_line:?}"
    );

    // Skip headers.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }

    let mut body = String::new();
    reader.read_to_string(&mut body).await.unwrap();

    assert!(
        body.contains(r#"manta_input_dropped_packets_total{source="hpsdr"} 9"#),
        "body: {body}"
    );
    assert!(
        body.contains(r#"manta_input_gaps_detected_total{source="hpsdr"} 2"#),
        "body: {body}"
    );
    assert!(
        body.contains(r#"manta_input_malformed_packets_total{source="hpsdr"} 4"#),
        "body: {body}"
    );
}

async fn get(addr: std::net::SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut status_line))
        .await
        .unwrap()
        .unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let mut body = String::new();
    reader.read_to_string(&mut body).await.unwrap();
    (status_line, body)
}

/// MAN-128 Scenario 2: `/healthz` flips from 200 to 503 the moment a
/// registered listener goes down, served over a real TCP connection (not
/// just the pure `route()` unit).
#[tokio::test]
async fn healthz_over_tcp_flips_from_200_to_503_when_a_listener_goes_down() {
    let metrics = Arc::new(Metrics::new());
    metrics.set_source_health("file", true);
    metrics.set_listener_up("telnet", true);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
    );
    let ip_quota = manta_server::tasks::IpQuota::new(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
    );
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            limiter,
            ip_quota,
            default_request_limiter(),
        )
        .await;
    });

    let (status, body) = get(addr, "/healthz").await;
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status:?}");
    assert!(body.starts_with("ok\n"), "body: {body}");

    metrics.set_listener_up("telnet", false);
    let (status, body) = get(addr, "/healthz").await;
    assert!(status.starts_with("HTTP/1.1 503"), "status: {status:?}");
    assert!(body.contains("listener telnet: down"), "body: {body}");
}

/// MAN-128 Scenario 2: the decode-stopped signal makes `/healthz` 503 even
/// though no source/listener check changed -- this is the window after
/// `listen_with_observers` returns and before the shutdown drain finishes,
/// where `source_health` alone would still read healthy.
#[tokio::test]
async fn healthz_reports_decode_stopped_over_tcp() {
    let metrics = Arc::new(Metrics::new());
    metrics.set_source_health("file", true);
    metrics.set_listener_up("telnet", true);
    metrics.arm_decode_watchdog();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
    );
    let ip_quota = manta_server::tasks::IpQuota::new(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
    );
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            limiter,
            ip_quota,
            default_request_limiter(),
        )
        .await;
    });

    let (status, _body) = get(addr, "/healthz").await;
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status:?}");

    metrics.mark_decode_stopped();
    let (status, body) = get(addr, "/healthz").await;
    assert!(status.starts_with("HTTP/1.1 503"), "status: {status:?}");
    assert!(body.contains("decode: stopped"), "body: {body}");
}

/// MAN-44 scenario 1+2 over the real wire: an operator hitting `/status`
/// sees per-target connection state and sent/suppressed/reconnect counts,
/// without reading logs.
#[tokio::test]
async fn operator_get_status_sees_uplink_connection_state_and_counts() {
    let metrics = Arc::new(Metrics::new());
    let target = metrics.register_uplink_target("rbn.example:7000".to_string(), true);
    target.mark_connected();
    target.record_sent();
    target.record_sent();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let metrics2 = metrics.clone();
    let limiter = manta_server::tasks::new_connection_limiter(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
    );
    let ip_quota = manta_server::tasks::IpQuota::new(
        manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
    );
    tokio::spawn(async move {
        manta_server::metrics_http::serve(
            listener,
            metrics2,
            limiter,
            ip_quota,
            default_request_limiter(),
        )
        .await;
    });

    let (status, body) = get(addr, "/status").await;
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status:?}");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["uplink"]["targets"][0]["target"], "rbn.example:7000");
    assert_eq!(doc["uplink"]["targets"][0]["connected"], true);
    assert_eq!(doc["uplink"]["targets"][0]["health"], "connected");
    assert_eq!(doc["uplink"]["sent_total"], 2);
    assert_eq!(doc["uplink"]["health"], "ok");
}
