//! MAN-12 acceptance scenario 3:
//!   Given manta is running as a daemon
//!   When an operator queries its metrics endpoint
//!   Then current spot rate, active track count, and per-source health are visible

use manta_server::metrics::{Metrics, UplinkTargetSpec};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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

/// MAN-44 scenario 1+2 over the real wire: an operator hitting `/status`
/// sees per-target connection state and sent/suppressed/reconnect counts,
/// without reading logs.
#[tokio::test]
async fn operator_get_status_sees_uplink_connection_state_and_counts() {
    let metrics = Arc::new(Metrics::new());
    let target = metrics.register_uplink_target(UplinkTargetSpec {
        label: "rbn.example:7000".to_string(),
        host: "rbn.example".to_string(),
        port: 7000,
        enabled: true,
        dry_run: false,
    });
    target.mark_connected();
    target.record_sent();
    target.record_sent();

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
        )
        .await;
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
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

    let mut content_type = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("content-type:") {
            content_type = line;
        }
    }
    assert!(
        content_type
            .to_ascii_lowercase()
            .contains("application/json"),
        "content-type: {content_type:?}"
    );

    let mut body = String::new();
    reader.read_to_string(&mut body).await.unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["uplink"]["targets"][0]["target"], "rbn.example:7000");
    assert_eq!(doc["uplink"]["targets"][0]["connected"], true);
    assert_eq!(doc["uplink"]["sent_total"], 2);
}
