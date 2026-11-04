//! MAN-142: `[[wsjtx_output]]` against a fake WSJT-X-aware logger on
//! loopback. The logger reads every datagram with `common::wsjtx`, a
//! reader written from WSJT-X's `NetworkMessage.hpp` that shares no code
//! with manta's encoder (D16). No test sends to a non-loopback address.

mod common;

use common::wsjtx::{self, Message};
use std::net::UdpSocket;
use std::time::Duration;

/// A fake logger's socket.
struct FakeLogger {
    socket: UdpSocket,
}

impl FakeLogger {
    fn bind(addr: &str) -> Self {
        let socket = UdpSocket::bind(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        Self { socket }
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    /// Every datagram already queued (or arriving within 500 ms of the
    /// last), read.
    fn drain(&self) -> Vec<Message> {
        let mut buf = [0u8; 65_536];
        let mut out = Vec::new();
        while let Ok(n) = self.socket.recv(&mut buf) {
            out.push(
                wsjtx::read(&buf[..n]).unwrap_or_else(|e| panic!("malformed WSJT-X datagram: {e}")),
            );
        }
        out
    }
}

/// manta never sends `QSO Logged` (5), `Logged ADIF` (12) or any type
/// beyond the four it writes.
fn assert_only_known_types(got: &[Message]) {
    for m in got {
        assert!(
            matches!(m.kind(), 0 | 1 | 2 | 6),
            "type {} in {got:?}",
            m.kind()
        );
    }
}

/// The worker in process, with a real `SpotBus` and real time.
mod worker {
    use super::*;
    use manta_server::bus::SpotBus;
    use manta_server::metrics::WsjtxOutputStats;
    use manta_server::wsjtx_output::{self, WsjtxOutput};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    const RATE: f64 = 48_000.0;
    const EPOCH: u64 = 1_793_650_000;

    fn spot(call: &str, freq_hz: f64, spot_type: manta_spot::SpotType) -> manta_spot::Spot {
        manta_spot::Spot {
            callsign: call.to_string(),
            freq_hz,
            snr_db: 21.0,
            wpm: 20.0,
            spot_type,
            confidence: 0.9,
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 21 * RATE as u64,
            rst: None,
            qrl_query: false,
            mode: manta_spot::SpotMode::Cw,
            baud: None,
        }
    }

    /// Runs the worker from `socket` to `destination`, publishes `spots`,
    /// then shuts it down and returns its counters.
    async fn run_with(
        socket: UdpSocket,
        destination: SocketAddr,
        spots: Vec<manta_spot::Spot>,
    ) -> Arc<WsjtxOutputStats> {
        socket.set_nonblocking(true).unwrap();
        let bus = Arc::new(SpotBus::new(
            RATE,
            UNIX_EPOCH + Duration::from_secs(EPOCH),
            7,
        ));
        let stats = Arc::new(WsjtxOutputStats::new(destination.to_string()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let rx = bus.subscribe();
        let task = tokio::spawn(wsjtx_output::run(
            WsjtxOutput {
                label: destination.to_string(),
                destination,
                de_call: "W5AU".to_string(),
                de_grid: "EM10dh".to_string(),
                version: "0.1.0".to_string(),
                revision: "d1ddf99a36ce".to_string(),
                off_air: false,
            },
            socket,
            bus.clone(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        for s in spots {
            bus.publish(s);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the worker ends after shutdown")
            .unwrap();
        stats
    }

    #[tokio::test]
    async fn a_live_spot_reads_as_heartbeat_status_decode_then_close() {
        let logger = FakeLogger::bind("127.0.0.1:0");
        let destination: SocketAddr = format!("127.0.0.1:{}", logger.port()).parse().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stats = run_with(
            socket,
            destination,
            vec![
                spot("W1AW", 14_012_340.0, manta_spot::SpotType::Cq),
                spot("K5ARH", 7_024_870.0, manta_spot::SpotType::De),
            ],
        )
        .await;
        let got = logger.drain();
        assert_only_known_types(&got);
        let for_20m: Vec<&Message> = got
            .iter()
            .filter(|m| match m {
                Message::Heartbeat { id, .. } | Message::Close { id } => id == "manta-20m",
                Message::Status(s) => s.id == "manta-20m",
                Message::Decode(d) => d.id == "manta-20m",
                Message::Unknown(_) => false,
            })
            .collect();
        assert!(for_20m.len() >= 4, "{got:?}");
        assert!(matches!(
            for_20m[0],
            Message::Heartbeat { max_schema: 3, version, revision, .. }
                if version == "0.1.0" && revision == "d1ddf99a36ce"
        ));
        let Message::Status(s) = for_20m[1] else {
            panic!("{got:?}")
        };
        assert_eq!(s.dial_freq_hz, 14_012_000);
        assert_eq!(s.mode, "CW");
        assert_eq!(s.tx_mode, "CW");
        assert_eq!((s.de_call.as_str(), s.de_grid.as_str()), ("W5AU", "EM10dh"));
        assert!(!s.tx_enabled && !s.transmitting && !s.decoding);
        assert_eq!(s.configuration_name, "manta");
        let Message::Decode(d) = for_20m[2] else {
            panic!("{got:?}")
        };
        assert_eq!(d.message, "CQ W1AW");
        assert_eq!(d.delta_freq_hz, 340);
        assert_eq!(d.snr, 21);
        assert_eq!(d.time_ms, 72_421_000);
        assert!(d.new && !d.low_confidence && !d.off_air);
        assert!(matches!(for_20m.last().unwrap(), Message::Close { .. }));
        // The 40 m spot is its own instance, read as `DE`.
        assert!(got.iter().any(|m| matches!(
            m,
            Message::Decode(d) if d.id == "manta-40m" && d.message == "DE K5ARH"
        )));
        assert!(got
            .iter()
            .any(|m| matches!(m, Message::Close { id } if id == "manta-40m")));
        assert_eq!(stats.spots(), 2);
        assert_eq!(stats.datagrams(), got.len() as u64);
    }

    /// The loopback broadcast address reaches a listener bound to the
    /// wildcard, which `SO_BROADCAST` on manta's socket allows (D13). The
    /// listener binds no externally reachable address on its own: the only
    /// datagram it can get here is the loopback broadcast.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_loopback_broadcast_reaches_a_wildcard_listener() {
        let logger = FakeLogger::bind("0.0.0.0:0");
        let destination: SocketAddr = format!("127.255.255.255:{}", logger.port())
            .parse()
            .unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_broadcast(true).unwrap();
        let stats = run_with(
            socket,
            destination,
            vec![spot("W1AW", 14_012_340.0, manta_spot::SpotType::Cq)],
        )
        .await;
        let got = logger.drain();
        assert_only_known_types(&got);
        assert!(
            got.iter()
                .any(|m| matches!(m, Message::Decode(d) if d.message == "CQ W1AW")),
            "{got:?}"
        );
        assert_eq!(stats.spots(), 1);
    }
}

/// `manta run` on a 30 s V1 replay that ends by itself.
mod manta_run {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A 30 s V1 IQ WAV at 48 kS/s, centred on 14 MHz: W1AW's CQ spot is
    /// decoded about 21 s in.
    fn v1_48k(dir: &Path) -> PathBuf {
        let spec = manta_testkit::vectors::VectorSpec {
            fs: 48_000.0,
            duration_s: 30.0,
            ..manta_testkit::vectors::v1()
        };
        manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
        dir.join(format!("{}.wav", spec.name))
    }

    #[test]
    fn manta_run_sends_a_replayed_spot_to_a_loopback_logger() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let logger = FakeLogger::bind("127.0.0.1:0");
        let config = format!(
            "[server]\nstation_callsign = \"W5AU\"\noperator_grid = \"EM10dh\"\n\
             bind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\n\
             json_port = 0\nmetrics_port = 0\nstatus_interval_secs = 0\n\n\
             [[wsjtx_output]]\ndestination = \"127.0.0.1:{}\"\n\n\
             [input]\ntype = \"file\"\npath = {:?}\niq = true\nreplay_epoch = 1793650000\n",
            logger.port(),
            wav.to_str().unwrap()
        );
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, config).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        for (key, _) in std::env::vars() {
            if key.starts_with("MANTA_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd.args(["run", "--config"]).arg(&path).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "wsjtx_output 127.0.0.1:{}: sending spots as WSJT-X Status and Decode messages \
                 for LAN loggers (one instance per band, \"manta-<band>\", heartbeat every 15 s)",
                logger.port()
            )),
            "{stderr}"
        );

        let got = logger.drain();
        assert_only_known_types(&got);
        let heartbeat = got
            .iter()
            .position(|m| matches!(m, Message::Heartbeat { id, .. } if id == "manta-20m"))
            .unwrap_or_else(|| panic!("no Heartbeat in {got:?}"));
        let status = got
            .iter()
            .position(|m| {
                matches!(
                    m,
                    Message::Status(s) if s.id == "manta-20m"
                        && s.dial_freq_hz == 14_012_000
                        && s.mode == "CW"
                        && s.de_call == "W5AU"
                        && s.de_grid == "EM10dh"
                )
            })
            .unwrap_or_else(|| panic!("no Status in {got:?}"));
        let decode = got
            .iter()
            .position(|m| {
                matches!(
                    m,
                    Message::Decode(d) if d.id == "manta-20m"
                        && d.message == "CQ W1AW"
                        && (290..=390).contains(&d.delta_freq_hz)
                        && d.new
                        && d.off_air
                )
            })
            .unwrap_or_else(|| panic!("no Decode in {got:?}"));
        let close = got
            .iter()
            .position(|m| matches!(m, Message::Close { id } if id == "manta-20m"))
            .unwrap_or_else(|| panic!("no Close in {got:?}"));
        assert!(
            heartbeat < status && status < decode && decode < close,
            "{got:?}"
        );
    }

    /// `enabled = false` keeps the block but sends nothing, while manta
    /// still validates the spot.
    #[test]
    fn manta_run_sends_nothing_from_a_disabled_block() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let logger = FakeLogger::bind("127.0.0.1:0");
        let config = format!(
            "[server]\nstation_callsign = \"W5AU\"\noperator_grid = \"EM10dh\"\n\
             bind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\n\
             json_port = 0\nmetrics_port = 0\nstatus_interval_secs = 0\n\n\
             [[wsjtx_output]]\ndestination = \"127.0.0.1:{}\"\nenabled = false\n\n\
             [input]\ntype = \"file\"\npath = {:?}\niq = true\nreplay_epoch = 1793650000\n",
            logger.port(),
            wav.to_str().unwrap()
        );
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, config).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        for (key, _) in std::env::vars() {
            if key.starts_with("MANTA_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd.args(["run", "--config"]).arg(&path).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("SPOT: W1AW"), "{stdout}\n{stderr}");
        assert!(
            !stderr.contains(&format!(
                "wsjtx_output 127.0.0.1:{}: sending spots",
                logger.port()
            )),
            "{stderr}"
        );
        let got = logger.drain();
        assert!(got.is_empty(), "{got:?}\n{stderr}");
    }
}
