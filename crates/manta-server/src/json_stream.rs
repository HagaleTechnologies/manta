//! JSON Lines spot stream, over plain TCP and WebSocket. ARCHITECTURE §7:
//! "full-fidelity spot objects... this is the cqdx ingest surface."

use crate::bus::SpotBus;
use crate::metrics::Metrics;
use crate::spot_message::SpotMessage;
use manta_spot::cty::Table;
use manta_spot::Spot;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

fn render(
    spot: &Spot,
    station_call: &str,
    cty: &Table,
    decoder_version: &str,
    unix_ts: i64,
) -> String {
    let msg = SpotMessage::from_spot(spot, station_call, cty, decoder_version, unix_ts);
    serde_json::to_string(&msg).expect("SpotMessage always serializes")
}

/// Plain-TCP JSON Lines server (one spot object per line, newline-delimited).
pub async fn serve_tcp(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let bus = bus.clone();
        let metrics = metrics.clone();
        let cty = cty.clone();
        let station_call = station_call.clone();
        let decoder_version = decoder_version.clone();
        tokio::spawn(async move {
            metrics.inc_json_clients();
            let _ = handle_tcp_client(socket, bus, cty, station_call, decoder_version).await;
            metrics.dec_json_clients();
        });
    }
}

async fn handle_tcp_client(
    mut socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) -> std::io::Result<()> {
    let mut rx = bus.subscribe();
    loop {
        match rx.recv().await {
            Ok(spot) => {
                let unix_ts = bus.unix_ts_for(spot.sample_ts);
                let line = render(&spot, &station_call, &cty, &decoder_version, unix_ts);
                socket.write_all(line.as_bytes()).await?;
                socket.write_all(b"\n").await?;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return Ok(()),
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

/// WebSocket JSON stream server (one spot object per text frame).
pub async fn serve_ws(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let bus = bus.clone();
        let metrics = metrics.clone();
        let cty = cty.clone();
        let station_call = station_call.clone();
        let decoder_version = decoder_version.clone();
        tokio::spawn(async move {
            metrics.inc_ws_clients();
            let _ = handle_ws_client(socket, bus, cty, station_call, decoder_version).await;
            metrics.dec_ws_clients();
        });
    }
}

async fn handle_ws_client(
    socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    cty: Arc<Table>,
    station_call: String,
    decoder_version: String,
) -> anyhow::Result<()> {
    use futures_util::SinkExt;

    let mut ws = tokio_tungstenite::accept_async(socket).await?;
    let mut rx = bus.subscribe();
    loop {
        match rx.recv().await {
            Ok(spot) => {
                let unix_ts = bus.unix_ts_for(spot.sample_ts);
                let text = render(&spot, &station_call, &cty, &decoder_version, unix_ts);
                ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                    .await?;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return Ok(()),
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}
