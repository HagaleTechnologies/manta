//! Minimal HTTP server exposing `Metrics::render_prometheus_text` on
//! `GET /metrics`. ARCHITECTURE §8: "Prometheus text endpoint (feature
//! `metrics`)." Hand-rolled rather than pulling in a full HTTP framework --
//! one static text response to one path is the entire surface.

use crate::bounded_io::read_line_bounded_with_timeout;
use crate::metrics::Metrics;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Publicly bound (ARCHITECTURE §7), so both bounds below matter: an
/// unauthenticated client must not be able to hold this connection's task
/// open by trickling headers, one every few seconds, forever.
const MAX_HEADER_LINES: usize = 100;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(listener: TcpListener, metrics: Arc<Metrics>) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let _ = handle_request(socket, metrics).await;
        });
    }
}

async fn handle_request(
    socket: tokio::net::TcpStream,
    metrics: Arc<Metrics>,
) -> std::io::Result<()> {
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    let mut request_line = String::new();
    if read_line_bounded_with_timeout(&mut reader, &mut request_line).await? == 0 {
        return Ok(());
    }
    // Drain the rest of the request headers; we don't need them.
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let n = read_line_bounded_with_timeout(&mut reader, &mut line).await?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    let body = if request_line.starts_with("GET /metrics ") {
        metrics.render_prometheus_text()
    } else {
        String::new()
    };
    let status = if request_line.starts_with("GET /metrics ") {
        "200 OK"
    } else {
        "404 Not Found"
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::time::timeout(WRITE_TIMEOUT, async {
        wr.write_all(response.as_bytes()).await?;
        wr.shutdown().await
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))??;
    Ok(())
}
