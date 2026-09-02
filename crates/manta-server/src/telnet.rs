//! Telnet DX-cluster server. ARCHITECTURE §7: "standard login prompt,
//! emits RBN-format spots... enough command grammar (`sh/dx`, filters) for
//! common clients not to choke." No real telnet IAC option negotiation --
//! real cluster nodes and clients (N1MM, stock `telnet`) work fine over
//! plain line-oriented text, and skipping IAC keeps this a small,
//! auditable text protocol (MAN-22/23 harden it further).

use crate::bus::SpotBus;
use crate::command::{self, Command};
use crate::metrics::Metrics;
use crate::rbn;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Accepts connections on `listener` until it errors, spawning one task
/// per client. Never returns under normal operation.
pub async fn serve(
    listener: TcpListener,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    station_call: String,
) {
    loop {
        let (socket, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let bus = bus.clone();
        let metrics = metrics.clone();
        let station_call = station_call.clone();
        tokio::spawn(async move {
            metrics.inc_telnet_clients();
            let _ = handle_client(socket, bus, station_call).await;
            metrics.dec_telnet_clients();
        });
    }
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    station_call: String,
) -> std::io::Result<()> {
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    // Subscribe before the login handshake, not after: a spot published
    // while the client is still typing its callsign must not be lost to
    // the broadcast channel's no-history-for-late-subscribers semantics.
    let mut rx = bus.subscribe();

    wr.write_all(b"login: \r\n").await?;
    let mut login_line = String::new();
    if reader.read_line(&mut login_line).await? == 0 {
        return Ok(()); // client hung up before logging in
    }

    wr.write_all(format!("de {station_call}-# >\r\n").as_bytes())
        .await?;

    // `sh/dx` default when the client didn't specify a count.
    const DEFAULT_SHOW_DX_COUNT: usize = 10;
    let mut min_unique: Option<u32> = None;
    let mut cmd_line = String::new();
    loop {
        cmd_line.clear();
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(spot) => {
                        if let Some(min) = min_unique {
                            if bus.occurrence_count(&spot.callsign) <= min {
                                continue; // below threshold: filtered out
                            }
                        }
                        write_spot_line(&mut wr, &bus, &station_call, &spot).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // ARCHITECTURE §7: slow clients are disconnected,
                        // never back-pressured.
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            n = reader.read_line(&mut cmd_line) => {
                if n? == 0 {
                    return Ok(()); // client disconnected
                }
                match command::parse(&cmd_line) {
                    Command::ShowDx { count } => {
                        let n = count.unwrap_or(DEFAULT_SHOW_DX_COUNT);
                        for spot in bus.recent(n) {
                            write_spot_line(&mut wr, &bus, &station_call, &spot).await?;
                        }
                    }
                    Command::SetFilterUnique { min } => {
                        min_unique = Some(min);
                        wr.write_all(format!("Filter set: unique > {min}\r\n").as_bytes())
                            .await?;
                    }
                    // Read-mostly protocol: any other line (unrecognized
                    // commands the client sent) is accepted without
                    // choking the connection.
                    Command::Unknown => {}
                }
            }
        }
    }
}

async fn write_spot_line(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    bus: &SpotBus,
    station_call: &str,
    spot: &manta_spot::Spot,
) -> std::io::Result<()> {
    let unix_ts = bus.unix_ts_for(spot.sample_ts);
    let line = rbn::format_line(spot, station_call, unix_ts);
    wr.write_all(line.as_bytes()).await?;
    wr.write_all(b"\r\n").await
}
