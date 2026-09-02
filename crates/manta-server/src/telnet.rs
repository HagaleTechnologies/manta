//! Telnet DX-cluster server. ARCHITECTURE §7: "standard login prompt,
//! emits RBN-format spots... enough command grammar (`sh/dx`, filters) for
//! common clients not to choke." No real telnet IAC option negotiation --
//! real cluster nodes and clients (N1MM, stock `telnet`) work fine over
//! plain line-oriented text, and skipping IAC keeps this a small,
//! auditable text protocol (MAN-22/23 harden it further).

use crate::bounded_io::read_line_bounded_with_timeout;
use crate::bus::SpotBus;
use crate::command::{self, Command};
use crate::metrics::Metrics;
use crate::rbn;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Every outbound write gets this long before the client is treated as
/// stalled and disconnected -- ARCHITECTURE §7's "slow clients are
/// disconnected, never back-pressured" policy applies to a client that
/// stops reading, not just one that falls behind the broadcast channel.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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
            let _ = handle_client(socket, bus, metrics.clone(), station_call).await;
            metrics.dec_telnet_clients();
        });
    }
}

async fn handle_client(
    socket: tokio::net::TcpStream,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    station_call: String,
) -> std::io::Result<()> {
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    // Subscribe before the login handshake, not after: a spot published
    // while the client is still typing its callsign must not be lost to
    // the broadcast channel's no-history-for-late-subscribers semantics.
    let mut rx = bus.subscribe();

    write_with_timeout(&mut wr, b"login: \r\n").await?;
    let mut login_line = String::new();
    if read_line_bounded_with_timeout(&mut reader, &mut login_line).await? == 0 {
        return Ok(()); // client hung up before logging in
    }

    write_with_timeout(&mut wr, format!("de {station_call}-# >\r\n").as_bytes()).await?;

    // `sh/dx` default when the client didn't specify a count.
    const DEFAULT_SHOW_DX_COUNT: usize = 10;
    let mut min_unique: Option<u32> = None;
    let mut cmd_line = String::new();
    loop {
        cmd_line.clear();
        tokio::select! {
            spot = rx.recv() => {
                match spot {
                    Ok(bus_spot) => {
                        if let Some(min) = min_unique {
                            if bus_spot.occurrence_count <= min {
                                continue; // below threshold: filtered out
                            }
                        }
                        write_spot_line(&mut wr, &bus, &station_call, &bus_spot.spot).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // ARCHITECTURE §7: slow clients are disconnected,
                        // never back-pressured -- and ARCHITECTURE §8:
                        // every dropped item is counted, not silent.
                        metrics.record_lagged(n);
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            n = read_line_bounded_with_timeout(&mut reader, &mut cmd_line) => {
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
                        write_with_timeout(&mut wr, format!("Filter set: unique > {min}\r\n").as_bytes())
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
    write_with_timeout(wr, line.as_bytes()).await?;
    write_with_timeout(wr, b"\r\n").await
}

async fn write_with_timeout(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    buf: &[u8],
) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))?
}
