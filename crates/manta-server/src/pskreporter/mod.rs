//! MAN-142: `[pskreporter]`, validated spots reported to PSKReporter
//! (`report.pskreporter.info:4739`) as IPFIX datagrams over UDP, paced by
//! the rules on PSKReporter's developer page
//! (<https://pskreporter.info/pskdev.html>, read 2026-11-02).
//!
//! [`ipfix`] writes the bytes, [`batch`] decides what goes in each
//! datagram and when, and [`run`] is the worker `manta run` spawns.

pub mod batch;
pub mod ipfix;

/// Why a validated spot was not reported; the `reason` label of
/// `manta_pskreporter_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PskDrop {
    /// Held back by `spot_types`.
    SpotType,
    /// The same call on the same band was reported less than 300 s earlier.
    RateGate,
    /// The worker fell behind the spot bus.
    Lagged,
    /// The host did not resolve when a datagram was due.
    ResolveError,
    /// The datagram's `send_to` failed.
    SendError,
}

impl PskDrop {
    /// Every reason, in render order.
    pub const ALL: [PskDrop; 5] = [
        PskDrop::SpotType,
        PskDrop::RateGate,
        PskDrop::Lagged,
        PskDrop::ResolveError,
        PskDrop::SendError,
    ];

    /// The `reason` label value.
    pub fn label(self) -> &'static str {
        match self {
            PskDrop::SpotType => "spot_type",
            PskDrop::RateGate => "rate_gate",
            PskDrop::Lagged => "lagged",
            PskDrop::ResolveError => "resolve_error",
            PskDrop::SendError => "send_error",
        }
    }
}

use crate::bus::{BusSpot, SpotBus};
use crate::config::UplinkSpotTypes;
use crate::metrics::PskReporterStats;
use crate::rbn_compare::Clock;
use batch::{Batcher, Datagram, Offer, FLUSH_INTERVAL};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

/// One `[pskreporter]` worker's settings, resolved by `manta-cli`.
#[derive(Debug, Clone)]
pub struct PskReporter {
    /// PSKReporter's host name or an IPv4 address, resolved when a
    /// datagram is due (D9).
    pub host: String,
    pub port: u16,
    /// The receiver information record (D8).
    pub receiver: ipfix::Receiver,
    pub spot_types: UplinkSpotTypes,
    /// Added to the first flush (D5), so stations started together do not
    /// send together.
    pub flush_offset: Duration,
    /// The header's observation domain ID (D7).
    pub observation_domain: u32,
}

/// D7: the session nonce folded to 32 bits, constant for the session.
pub fn observation_domain(nonce: u128) -> u32 {
    let n = nonce as u64 ^ (nonce >> 64) as u64;
    (n as u32) ^ ((n >> 32) as u32)
}

/// D5: the first flush's offset past 300 s, `nonce % 30` seconds.
pub fn flush_offset(nonce: u128) -> Duration {
    Duration::from_secs((nonce % 30) as u64)
}

/// How long one lookup of `host` may take. The worker reads no spots
/// while it waits, so a stuck resolver must not stall it for long.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The first IPv4 address `host:port` resolves to.
async fn resolve(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| std::io::Error::other("the lookup timed out"))??
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| std::io::Error::other("no IPv4 address"))
}

/// The socket side: resolution, sends, counters and the warn-once state.
struct Sender {
    host: String,
    port: u16,
    socket: tokio::net::UdpSocket,
    addr: Option<SocketAddr>,
    stats: Arc<PskReporterStats>,
    warned_resolve: bool,
    warned_send: bool,
}

impl Sender {
    /// Sends `d`, or counts its reports as lost. Returns whether it was
    /// handed to the OS.
    async fn send(&mut self, d: Datagram) -> bool {
        let n = d.reports as u64;
        // D9: resolve before the first send and with each template resend.
        // A failed re-resolution keeps the last address that worked.
        if self.addr.is_none() || d.templates {
            match resolve(&self.host, self.port).await {
                Ok(addr) => self.addr = Some(addr),
                Err(_) if self.addr.is_some() => {}
                Err(e) => {
                    self.stats.record_dropped(PskDrop::ResolveError, n);
                    if !self.warned_resolve {
                        self.warned_resolve = true;
                        tracing::warn!(
                            "pskreporter: cannot resolve {}: {e}; manta tries again at the next \
                             send and counts the lost reports in \
                             manta_pskreporter_dropped_total{{reason=\"resolve_error\"}}",
                            self.host
                        );
                    }
                    return false;
                }
            }
        }
        let Some(addr) = self.addr else {
            return false;
        };
        match self.socket.send_to(&d.bytes, addr).await {
            Ok(_) => {
                self.stats.record_datagram(n);
                true
            }
            Err(e) => {
                self.stats.record_dropped(PskDrop::SendError, n);
                if !self.warned_send {
                    self.warned_send = true;
                    tracing::warn!(
                        "pskreporter: cannot send to {addr}: {e}; manta keeps trying and counts \
                         the lost reports in manta_pskreporter_dropped_total{{reason=\"send_error\"}}"
                    );
                }
                false
            }
        }
    }
}

/// Sends `d`; templates that did not reach the OS are due again.
async fn deliver(batcher: &mut Batcher, sender: &mut Sender, d: Datagram) {
    let templates = d.templates;
    if !sender.send(d).await && templates {
        batcher.templates_not_sent();
    }
}

fn export_time(clock: &Clock) -> u32 {
    clock().clamp(0, i64::from(u32::MAX)) as u32
}

async fn offer(
    batcher: &mut Batcher,
    sender: &mut Sender,
    bus: &SpotBus,
    clock: &Clock,
    spot: &BusSpot,
) {
    let unix_ts = bus.unix_ts_for(spot.spot.sample_ts);
    match batcher.offer(&spot.spot, unix_ts, Instant::now(), export_time(clock)) {
        Offer::Admitted => {}
        Offer::Dropped(reason) => sender.stats.record_dropped(reason, 1),
        Offer::Full(d) => deliver(batcher, sender, d).await,
    }
}

/// Runs `[pskreporter]` over a pre-bound (nonblocking) socket until
/// shutdown; then drains `rx`, sends what is pending and returns. It
/// never sends without a pending report.
pub async fn run(
    cfg: PskReporter,
    socket: std::net::UdpSocket,
    clock: Clock,
    bus: Arc<SpotBus>,
    mut rx: broadcast::Receiver<BusSpot>,
    stats: Arc<PskReporterStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let socket = match tokio::net::UdpSocket::from_std(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("pskreporter: cannot use the socket: {e}");
            return;
        }
    };
    let mut batcher = Batcher::new(cfg.receiver, cfg.spot_types, cfg.observation_domain);
    let mut sender = Sender {
        host: cfg.host,
        port: cfg.port,
        socket,
        addr: None,
        stats,
        warned_resolve: false,
        warned_send: false,
    };
    let mut next_flush = Instant::now() + FLUSH_INTERVAL + cfg.flush_offset;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(next_flush) => {
                next_flush += FLUSH_INTERVAL;
                while let Some(d) = batcher.flush(Instant::now(), export_time(&clock)) {
                    deliver(&mut batcher, &mut sender, d).await;
                }
            }
            received = rx.recv() => match received {
                Ok(spot) => offer(&mut batcher, &mut sender, &bus, &clock, &spot).await,
                Err(RecvError::Lagged(n)) => sender.stats.record_dropped(PskDrop::Lagged, n),
                Err(RecvError::Closed) => break,
            },
        }
    }
    // Spots published before shutdown still count.
    loop {
        match rx.try_recv() {
            Ok(spot) => offer(&mut batcher, &mut sender, &bus, &clock, &spot).await,
            Err(TryRecvError::Lagged(n)) => sender.stats.record_dropped(PskDrop::Lagged, n),
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    while let Some(d) = batcher.flush(Instant::now(), export_time(&clock)) {
        deliver(&mut batcher, &mut sender, d).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::{Spot, SpotType};
    use std::time::UNIX_EPOCH;
    use tokio::net::UdpSocket;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    const RATE: f64 = 48_000.0;
    const EPOCH: u64 = 1_793_650_000;
    const NOW: i64 = 1_793_650_330;
    const OFFSET: Duration = Duration::from_secs(7);

    fn spot(call: &str, at_s: u64, snr_db: f32, spot_type: SpotType) -> Spot {
        Spot {
            callsign: call.to_string(),
            freq_hz: 14_012_340.0,
            snr_db,
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

    fn receiver() -> ipfix::Receiver {
        ipfix::Receiver {
            callsign: "W5AU".into(),
            locator: "EM10dh".into(),
            software: "manta 0.1.0".into(),
            antenna: "test dipole".into(),
        }
    }

    struct Harness {
        bus: Arc<SpotBus>,
        stats: Arc<PskReporterStats>,
        collector: UdpSocket,
        shutdown: watch::Sender<bool>,
        rx: Option<broadcast::Receiver<BusSpot>>,
        port: u16,
        host: String,
    }

    impl Harness {
        async fn new(host: &str) -> Self {
            let collector = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = collector.local_addr().unwrap().port();
            let bus = Arc::new(SpotBus::new(
                RATE,
                UNIX_EPOCH + Duration::from_secs(EPOCH),
                42,
            ));
            let rx = Some(bus.subscribe());
            Self {
                bus,
                stats: Arc::new(PskReporterStats::default()),
                collector,
                shutdown: watch::channel(false).0,
                rx,
                port,
                host: host.to_string(),
            }
        }

        fn start(&mut self) -> JoinHandle<()> {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_nonblocking(true).unwrap();
            let clock: Clock = Arc::new(|| NOW);
            tokio::spawn(run(
                PskReporter {
                    host: self.host.clone(),
                    port: self.port,
                    receiver: receiver(),
                    spot_types: UplinkSpotTypes::CqBeacon,
                    flush_offset: OFFSET,
                    observation_domain: 0x1234_ABCD,
                },
                socket,
                clock,
                self.bus.clone(),
                self.rx.take().expect("started once"),
                self.stats.clone(),
                self.shutdown.subscribe(),
            ))
        }

        async fn recv(&self, within: Duration) -> Option<Vec<u8>> {
            let mut buf = vec![0u8; 2048];
            match timeout(within, self.collector.recv(&mut buf)).await {
                Ok(Ok(n)) => Some(buf[..n].to_vec()),
                Ok(Err(e)) => panic!("recv: {e}"),
                Err(_) => None,
            }
        }
    }

    fn templated(d: &[u8]) -> bool {
        u16::from_be_bytes([d[16], d[17]]) == 3
    }

    fn sequence(d: &[u8]) -> u32 {
        u32::from_be_bytes([d[8], d[9], d[10], d[11]])
    }

    #[test]
    fn the_observation_domain_folds_the_nonce_and_the_offset_is_under_30_s() {
        assert_eq!(observation_domain(0), 0);
        assert_eq!(observation_domain(0x1234_ABCD), 0x1234_ABCD);
        assert_eq!(observation_domain(1u128 << 64), 1);
        assert_eq!(observation_domain(1u128 << 96), 1);
        assert_eq!(flush_offset(29), Duration::from_secs(29));
        assert_eq!(flush_offset(30), Duration::ZERO);
        assert!(flush_offset(u128::MAX) < Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_before_the_first_flush_and_one_datagram_is_sent_at_it() {
        let mut h = Harness::new("127.0.0.1").await;
        let _task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        assert_eq!(
            h.recv(FLUSH_INTERVAL + OFFSET - Duration::from_secs(1))
                .await,
            None
        );
        let d = h
            .recv(Duration::from_secs(5))
            .await
            .expect("the first flush");
        let want = ipfix::datagram(
            &ipfix::Header {
                export_time: NOW as u32,
                sequence: 0,
                observation_domain: 0x1234_ABCD,
            },
            &receiver(),
            true,
            &[ipfix::Report {
                callsign: "W1AW".into(),
                freq_hz: 14_012_340,
                snr_db: 21,
                mode: "CW",
                flow_start: (EPOCH + 21) as u32,
            }],
        );
        assert_eq!(d, want);
        // Nothing pending: nothing more is sent.
        assert_eq!(h.recv(FLUSH_INTERVAL * 2).await, None);
        assert_eq!(h.stats.datagrams(), 1);
        assert_eq!(h.stats.reports(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_three_datagrams_carry_templates_and_the_fourth_does_not() {
        let mut h = Harness::new("127.0.0.1").await;
        let _task = h.start();
        let mut seen = Vec::new();
        for i in 0..4u64 {
            h.bus
                .publish(spot(&format!("K{i}ABC"), 21 + 300 * i, 15.0, SpotType::Cq));
            let d = h
                .recv(FLUSH_INTERVAL + OFFSET + Duration::from_secs(5))
                .await;
            let d = d.expect("a datagram per flush");
            seen.push((templated(&d), sequence(&d)));
        }
        // Each datagram adds its report and its receiver record.
        assert_eq!(seen, vec![(true, 0), (true, 2), (true, 4), (false, 6)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_re_emitted_call_is_reported_once() {
        let mut h = Harness::new("127.0.0.1").await;
        let _task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        sleep(Duration::from_secs(60)).await;
        h.bus.publish(spot("W1AW", 81, 27.0, SpotType::Cq));
        let d = h
            .recv(FLUSH_INTERVAL + OFFSET)
            .await
            .expect("the first flush");
        assert_eq!(h.stats.reports(), 1);
        assert_eq!(h.stats.dropped(PskDrop::RateGate), 1);
        // One report: header, templates, receiver record, one sender record.
        let one = ipfix::datagram_len(
            &receiver(),
            true,
            ipfix::report_len(&batch::report_for(&spot("W1AW", 21, 21.0, SpotType::Cq), 0)),
        );
        assert_eq!(d.len(), one);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_sends_what_is_pending() {
        let mut h = Harness::new("127.0.0.1").await;
        let task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        sleep(Duration::from_secs(10)).await;
        h.shutdown.send(true).unwrap();
        let d = h
            .recv(Duration::from_secs(1))
            .await
            .expect("the final flush");
        assert!(templated(&d));
        timeout(Duration::from_secs(1), task)
            .await
            .expect("the worker ends")
            .unwrap();
        assert_eq!(h.stats.reports(), 1);
        assert_eq!(h.stats.datagrams(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spot_published_before_the_task_first_runs_is_reported() {
        let mut h = Harness::new("127.0.0.1").await;
        // The receiver was subscribed in `new`, before the spawn (D3).
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        let _task = h.start();
        let d = h
            .recv(FLUSH_INTERVAL + OFFSET + Duration::from_secs(1))
            .await;
        assert!(d.is_some());
        assert_eq!(h.stats.reports(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn sending_to_a_closed_port_is_not_an_error() {
        let mut h = Harness::new("127.0.0.1").await;
        // Nothing listens on the collector's port any more.
        let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        h.port = closed.local_addr().unwrap().port();
        drop(closed);
        let task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        sleep(FLUSH_INTERVAL + OFFSET + Duration::from_secs(1)).await;
        h.shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(h.stats.datagrams(), 1);
        assert_eq!(h.stats.reports(), 1);
        assert_eq!(h.stats.dropped(PskDrop::SendError), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_does_not_resolve_counts_resolve_error() {
        // RFC 6761: `.invalid` never resolves, so nothing is sent anywhere.
        let mut h = Harness::new("pskreporter.invalid").await;
        let task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        h.bus.publish(spot("K1ABC", 22, 15.0, SpotType::Cq));
        sleep(Duration::from_secs(1)).await;
        h.shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(h.stats.dropped(PskDrop::ResolveError), 2);
        assert_eq!(h.stats.datagrams(), 0);
        assert_eq!(h.stats.reports(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_counters_match_what_was_sent() {
        let mut h = Harness::new("127.0.0.1").await;
        let task = h.start();
        h.bus.publish(spot("W1AW", 21, 21.0, SpotType::Cq));
        h.bus.publish(spot("K5ARH", 22, 14.0, SpotType::De));
        h.bus.publish(spot("W1AW", 81, 27.0, SpotType::Cq));
        h.bus.publish(spot("K1ABC", 90, 9.0, SpotType::Beacon));
        let d = h
            .recv(FLUSH_INTERVAL + OFFSET + Duration::from_secs(1))
            .await;
        let d = d.expect("the first flush");
        h.shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(h.stats.datagrams(), 1);
        assert_eq!(h.stats.reports(), 2);
        assert_eq!(h.stats.dropped(PskDrop::SpotType), 1);
        assert_eq!(h.stats.dropped(PskDrop::RateGate), 1);
        for reason in [PskDrop::Lagged, PskDrop::ResolveError, PskDrop::SendError] {
            assert_eq!(h.stats.dropped(reason), 0, "{reason:?}");
        }
        assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len());
    }
}
