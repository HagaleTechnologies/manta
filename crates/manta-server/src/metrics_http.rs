//! Minimal HTTP server exposing `Metrics::render_prometheus_text` on
//! `GET /metrics`. ARCHITECTURE §8: "Prometheus text endpoint (feature
//! `metrics`)." Hand-rolled rather than pulling in a full HTTP framework --
//! one static text response to one path is the entire surface.

use crate::metrics::Metrics;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

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
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    // Drain the rest of the request headers; we don't need them.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
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
    wr.write_all(response.as_bytes()).await?;
    wr.shutdown().await?;
    Ok(())
}
