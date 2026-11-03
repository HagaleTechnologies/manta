//! MAN-142: `[pskreporter]` against a fake PSKReporter collector on
//! loopback. The collector reads every datagram with `common::ipfix`, a
//! reader written from PSKReporter's developer page that shares no code
//! with manta's encoder (D16). No test sends to a non-loopback address:
//! the replay-guard test names `192.0.2.1` (TEST-NET-1, never routed), and
//! with the guard working no socket is opened for it at all.

mod common;

use common::ipfix::{element, Collector, Datagram, Element};
use std::net::UdpSocket;
use std::time::Duration;

/// A fake collector on `127.0.0.1:0`.
struct FakeCollector {
    socket: UdpSocket,
}

impl FakeCollector {
    fn bind() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        Self { socket }
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    /// Every datagram already queued (or arriving within 500 ms of the
    /// last), read through one collector session.
    fn drain(&self) -> Vec<Datagram> {
        let mut collector = Collector::default();
        let mut buf = [0u8; 65_536];
        let mut out = Vec::new();
        while let Ok(n) = self.socket.recv(&mut buf) {
            out.push(
                collector
                    .read(&buf[..n])
                    .unwrap_or_else(|e| panic!("malformed IPFIX datagram: {e}")),
            );
        }
        out
    }
}

mod reader {
    use super::common::ipfix::{element, Collector, Element};

    /// The first complete example datagram on
    /// <https://pskreporter.info/pskdev.html> (read 2026-11-02): N1DQ at
    /// FN42hn hearing N1DQ and KB1MBX in PSK.
    const PAGE_EXAMPLE: &str = "
        00 0A 00 AC 47 95 32 72 00 00 00 01 00 00 00 00
        00 03 00 24 99 92 00 03 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F
        80 08 FF FF 00 00 76 8F 00 00
        00 02 00 2C 99 93 00 05 80 01 FF FF 00 00 76 8F 80 05 00 04 00 00 76 8F
        80 0A FF FF 00 00 76 8F 80 0B 00 01 00 00 76 8F 00 96 00 04
        99 92 00 20 04 4E 31 44 51 06 46 4E 34 32 68 6E
        0D 48 6F 6D 65 62 72 65 77 20 76 35 2E 36 00 00
        99 93 00 2C 04 4E 31 44 51 00 D6 B3 27 03 50 53 4B 01 47 95 32 54
        06 4B 42 31 4D 42 58 00 D6 B4 CB 03 50 53 4B 01 47 95 32 68
        00 00";

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn the_reader_reads_the_developer_pages_example() {
        let bytes = hex(PAGE_EXAMPLE);
        assert_eq!(bytes.len(), 172);
        let d = Collector::default().read(&bytes).unwrap();
        assert_eq!(d.header.export_time, 0x4795_3272);
        assert_eq!(d.options_templates, [0x9992]);
        assert_eq!(d.templates, [0x9993]);
        assert_eq!(d.receivers.len(), 1);
        let rx = &d.receivers[0];
        assert_eq!(rx[&Element::psk(element::RECEIVER_CALLSIGN)].str(), "N1DQ");
        assert_eq!(rx[&Element::psk(element::RECEIVER_LOCATOR)].str(), "FN42hn");
        assert_eq!(
            rx[&Element::psk(element::DECODING_SOFTWARE)].str(),
            "Homebrew v5.6"
        );
        let calls: Vec<&str> = d
            .senders
            .iter()
            .map(|r| r[&Element::psk(element::SENDER_CALLSIGN)].str())
            .collect();
        assert_eq!(calls, ["N1DQ", "KB1MBX"]);
        assert_eq!(
            d.senders[1][&Element::psk(element::FREQUENCY)].uint(),
            14_070_987
        );
        assert_eq!(d.senders[0][&Element::psk(element::MODE)].str(), "PSK");
        assert_eq!(
            d.senders[0][&Element::iana(element::FLOW_START_SECONDS)].uint(),
            1_200_960_084
        );
    }

    #[test]
    fn the_reader_refuses_a_data_set_without_its_template() {
        let bytes = hex(PAGE_EXAMPLE);
        // The header plus the receiver data set alone, length patched.
        let mut cut = bytes[..16].to_vec();
        cut.extend_from_slice(&bytes[16 + 36 + 44..16 + 36 + 44 + 32]);
        let len = cut.len() as u16;
        cut[2..4].copy_from_slice(&len.to_be_bytes());
        assert!(Collector::default().read(&cut).is_err());
    }
}

/// The worker in process, against the fake collector, with a real
/// `SpotBus` and real time: shutdown flushes what is pending.
mod worker {
    use super::*;
    use manta_server::bus::SpotBus;
    use manta_server::metrics::PskReporterStats;
    use manta_server::pskreporter::{self, ipfix, PskDrop};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const RATE: f64 = 48_000.0;
    const EPOCH: u64 = 1_793_650_000;

    fn spot(call: &str, at_s: u64, spot_type: manta_spot::SpotType) -> manta_spot::Spot {
        manta_spot::Spot {
            callsign: call.to_string(),
            freq_hz: 14_012_340.0,
            snr_db: 21.0,
            wpm: 20.0,
            spot_type,
            confidence: 0.9,
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 1,
            sample_ts: at_s * RATE as u64,
            rst: None,
            qrl_query: false,
            mode: manta_spot::SpotMode::Cw,
            baud: None,
        }
    }

    #[tokio::test]
    async fn shutdown_sends_what_is_pending_in_the_published_format() {
        let collector = FakeCollector::bind();
        let bus = Arc::new(SpotBus::new(
            RATE,
            UNIX_EPOCH + Duration::from_secs(EPOCH),
            7,
        ));
        let stats = Arc::new(PskReporterStats::default());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let rx = bus.subscribe();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let task = tokio::spawn(pskreporter::run(
            pskreporter::PskReporter {
                host: "127.0.0.1".to_string(),
                port: collector.port(),
                receiver: ipfix::Receiver {
                    callsign: "W5AU".to_string(),
                    locator: "EM10dh".to_string(),
                    software: "manta 0.1.0".to_string(),
                    antenna: String::new(),
                },
                spot_types: manta_server::config::UplinkSpotTypes::CqBeacon,
                flush_offset: Duration::from_secs(7),
                observation_domain: 0x1234_abcd,
            },
            socket,
            Arc::new(move || now),
            bus.clone(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        bus.publish(spot("W1AW", 21, manta_spot::SpotType::Cq));
        bus.publish(spot("K5ARH", 30, manta_spot::SpotType::De));
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Nothing before the first flush, 300 s plus the offset away.
        assert!(collector.drain().is_empty());
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the worker ends after shutdown")
            .unwrap();

        let got = collector.drain();
        assert_eq!(got.len(), 1, "{got:?}");
        let d = &got[0];
        assert_eq!(d.header.sequence, 0);
        assert_eq!(d.header.observation_domain, 0x1234_abcd);
        assert_eq!(d.header.export_time as i64, now);
        assert_eq!(d.options_templates.len(), 1);
        assert_eq!(d.templates.len(), 1);
        assert_eq!(d.receivers.len(), 1);
        assert_eq!(
            d.receivers[0][&Element::psk(element::RECEIVER_CALLSIGN)].str(),
            "W5AU"
        );
        assert_eq!(
            d.receivers[0][&Element::psk(element::ANTENNA_INFORMATION)].str(),
            ""
        );
        // `cq_beacon` holds the DE spot back.
        assert_eq!(d.senders.len(), 1);
        let s = &d.senders[0];
        assert_eq!(s[&Element::psk(element::SENDER_CALLSIGN)].str(), "W1AW");
        assert_eq!(s[&Element::psk(element::FREQUENCY)].uint(), 14_012_340);
        assert_eq!(s[&Element::psk(element::SNR)].uint(), 21);
        assert_eq!(
            s[&Element::iana(element::FLOW_START_SECONDS)].uint(),
            EPOCH + 21
        );
        assert_eq!(stats.datagrams(), 1);
        assert_eq!(stats.reports(), 1);
        assert_eq!(stats.dropped(PskDrop::SpotType), 1);
    }
}

/// `manta run` on a 30 s V1 replay that ends by itself.
mod manta_run {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

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

    /// `manta run --config <config>` with no inherited MANTA_* variables.
    fn manta_run(dir: &Path, config: &str) -> Output {
        let path = dir.join("manta.toml");
        std::fs::write(&path, config).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        for (key, _) in std::env::vars() {
            if key.starts_with("MANTA_") {
                cmd.env_remove(key);
            }
        }
        cmd.args(["run", "--config"]).arg(&path);
        cmd.output().unwrap()
    }

    /// `[server]` with every port 0 on loopback, `[pskreporter]` with
    /// `host`/`port`, and the replay.
    fn config(wav: &Path, host: &str, port: u16) -> String {
        format!(
            "[server]\nstation_callsign = \"W5AU\"\noperator_grid = \"EM10dh\"\n\
             bind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = 0\n\
             json_port = 0\nmetrics_port = 0\nstatus_interval_secs = 0\n\n\
             [pskreporter]\nenabled = true\nhost = \"{host}\"\nport = {port}\n\
             antenna = \"test dipole\"\n\n\
             [input]\ntype = \"file\"\npath = {:?}\niq = true\nreplay_epoch = 1793650000\n",
            wav.to_str().unwrap()
        )
    }

    #[test]
    fn manta_run_reports_a_replayed_spot_to_a_loopback_collector() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        let collector = FakeCollector::bind();
        let out = manta_run(dir.path(), &config(&wav, "127.0.0.1", collector.port()));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "pskreporter: reporting cq_beacon spots to 127.0.0.1:{} as W5AU at EM10dh; \
                 reports are public;",
                collector.port()
            )),
            "{stderr}"
        );

        let got = collector.drain();
        assert_eq!(got.len(), 1, "{got:?}\n{stderr}");
        let d = &got[0];
        assert!(!d.templates.is_empty() && !d.options_templates.is_empty());
        assert_eq!(d.receivers.len(), 1);
        let rx = &d.receivers[0];
        assert_eq!(rx[&Element::psk(element::RECEIVER_CALLSIGN)].str(), "W5AU");
        assert_eq!(rx[&Element::psk(element::RECEIVER_LOCATOR)].str(), "EM10dh");
        assert_eq!(
            rx[&Element::psk(element::DECODING_SOFTWARE)].str(),
            format!("manta {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            rx[&Element::psk(element::ANTENNA_INFORMATION)].str(),
            "test dipole"
        );
        assert_eq!(d.senders.len(), 1, "{:?}", d.senders);
        let s = &d.senders[0];
        assert_eq!(s[&Element::psk(element::SENDER_CALLSIGN)].str(), "W1AW");
        let freq = s[&Element::psk(element::FREQUENCY)].uint();
        assert!(freq.abs_diff(14_012_340) <= 50, "frequency {freq}");
        assert_eq!(s[&Element::psk(element::MODE)].str(), "CW");
        assert_eq!(s[&Element::psk(element::INFORMATION_SOURCE)].uint(), 1);
        assert_eq!(s[&Element::psk(element::SENDER_LOCATOR)].str(), "");
        let t = s[&Element::iana(element::FLOW_START_SECONDS)].uint();
        assert!(
            (1_793_650_010..=1_793_650_030).contains(&t),
            "flowStart {t}"
        );
    }

    #[test]
    fn manta_run_does_not_report_a_replay_to_a_non_loopback_host() {
        let dir = tempfile::tempdir().unwrap();
        let wav = v1_48k(dir.path());
        // TEST-NET-1 (RFC 5737): never routed, and the guard opens no
        // socket for it.
        let out = manta_run(dir.path(), &config(&wav, "192.0.2.1", 4739));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains(
                "note: [pskreporter] is off for file replay: 192.0.2.1 is not a loopback \
                 address, and a recording's spots must not reach PSKReporter's public map"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("pskreporter: reporting"), "{stderr}");
    }
}
