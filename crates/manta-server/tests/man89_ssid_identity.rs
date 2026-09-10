//! MAN-89 / D4 acceptance:
//!   Given an operator configures station_callsign as "W5AU-1"
//!   When manta validates the config
//!   Then it accepts the value and the node identifies itself as W5AU-1-#
//!
//! Deliberately asserts LITERAL wire bytes rather than comparing against
//! `rbn::format_line`'s own output the way `telnet_acceptance.rs` does -- a
//! self-comparison cannot catch a regression in the `-#` append itself.

use manta_server::bus::SpotBus;
use manta_server::config::ServerConfig;
use manta_server::metrics::Metrics;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const SAMPLE_RATE_HZ: f64 = 96_000.0;

async fn spawn_server(
    station_call: &str,
) -> (
    std::net::SocketAddr,
    Arc<SpotBus>,
    tokio::sync::watch::Sender<bool>,
    manta_server::tasks::ClientTasks,
) {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let bus2 = bus.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();
    let tasks_handle = tasks.clone();
    let limiter =
        manta_server::tasks::new_connection_limiter(manta_server::telnet::MAX_TELNET_CONNECTIONS);
    let station_call = station_call.to_string();
    tokio::spawn(async move {
        manta_server::telnet::serve(
            listener,
            bus2,
            metrics,
            station_call,
            shutdown_rx,
            tasks,
            limiter,
            manta_server::tasks::IpQuota::new(manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP),
            manta_server::rate_limit::IpRateLimiter::new(
                manta_server::telnet::MAX_TELNET_COMMANDS,
                manta_server::telnet::COMMAND_RATE_WINDOW,
            ),
        )
        .await;
    });

    (addr, bus, shutdown_tx, tasks_handle)
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

#[tokio::test]
async fn a_per_band_ssid_identity_reaches_the_telnet_wire_as_call_n_hash() {
    // Given: the operator's config (lowercase, to pin normalization too).
    let cfg: ServerConfig = toml::from_str(r#"station_callsign = "w5au-1""#)
        .expect("a per-band SSID must be accepted by config validation");
    assert_eq!(cfg.station_callsign, "W5AU-1");

    let (addr, bus, _shutdown_tx, _tasks) = spawn_server(&cfg.station_callsign).await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    // Bounded for the same reason as the reads below (PR #131 review round 2):
    // if the server accepts the connection but regresses before writing its
    // login prompt, a bare `read_line` blocks forever and holds the whole test
    // binary until the runner's global timeout instead of reporting the
    // handshake failure.
    let mut login_prompt = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut login_prompt))
        .await
        .expect("timed out waiting for the login prompt")
        .unwrap();
    assert_ne!(n, 0, "connection closed before the login prompt");
    assert!(
        login_prompt.to_lowercase().contains("login")
            || login_prompt.to_lowercase().contains("call"),
        "expected a login prompt, got: {login_prompt:?}"
    );

    wr.write_all(b"N0CALL\r\n").await.unwrap();

    // Then: the node's post-login prompt carries the -N-# identity.
    // Bounded and EOF-checked (PR #131 review): an unbounded `read_line` loop
    // spins forever on `Ok(0)` if a regression closes the connection after
    // login, holding the whole test binary until the runner's global timeout
    // instead of reporting the regression.
    let station_prompt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert_ne!(n, 0, "connection closed before the station prompt");
            if line.contains(&cfg.station_callsign) {
                break line;
            }
        }
    })
    .await
    .expect("timed out waiting for the station prompt");
    assert_eq!(station_prompt, "de W5AU-1-# >\r\n");

    // ... and so does every spot line.
    bus.publish(sample_spot());

    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for spot line")
        .unwrap();
    assert_ne!(n, 0, "connection closed before the spot line");
    assert!(line.starts_with("DX de W5AU-1-#:"), "{line:?}");

    // MAN-88 Decision 3's invariant, asserted layout-independently: the
    // identity is never truncated and never abuts the next field.
    let after = line.trim_end().strip_prefix("DX de W5AU-1-#:").unwrap();
    assert!(
        after.starts_with(' '),
        "identity ran into the frequency: {line:?}"
    );
}
