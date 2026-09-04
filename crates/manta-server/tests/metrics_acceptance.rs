//! MAN-12 acceptance scenario 3:
//!   Given manta is running as a daemon
//!   When an operator queries its metrics endpoint
//!   Then current spot rate, active track count, and per-source health are visible
//!
//! Also covers MAN-64 (PR #76 review round 7): the per-source-IP budget on
//! COMPLETED requests, distinct from MAN-61's per-IP CONNECTION quota above
//! -- see `completed_requests_from_one_ip_are_rate_limited`.

use manta_server::metrics::Metrics;
use manta_server::rate_limit::IpRateLimiter;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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
    metrics.record_spot();
    metrics.record_spot();
    metrics.record_spot();
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
