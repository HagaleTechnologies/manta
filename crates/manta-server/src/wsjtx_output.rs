//! MAN-142: `[[wsjtx_output]]`, validated spots sent to a LAN address as
//! WSJT-X `NetworkMessage` datagrams (schema 3), for loggers that already
//! listen for WSJT-X.
//!
//! Exactly four message types go out (D12): `Heartbeat` (0) the first time
//! an `Id` is used and every [`PULSE`] after, `Status` (1) before a
//! `Decode` whose dial or mode differs from that `Id`'s last `Status` and
//! after every heartbeat, `Decode` (2) once per spot, and `Close` (6) per
//! `Id` at shutdown. There is deliberately no writer for `QSO Logged` (5)
//! or `Logged ADIF` (12): loggers write those into the log as contacts.
//!
//! Each band is its own instance, `Id` = `manta-<band>`, so a `Status` and
//! its `Decode` never interleave with another band's. A reader's RF
//! frequency is the `Status` dial plus the `Decode` delta frequency. The
//! layout follows WSJT-X `Network/NetworkMessage.hpp` (read 2026-11-02);
//! every value is a big-endian Qt `QDataStream` field. MAN-142's decision
//! record in `docs/DECISIONS/` has the reasons.

use crate::band::band_for_freq_hz;
use crate::bus::{BusSpot, SpotBus};
use crate::metrics::WsjtxOutputStats;
use manta_spot::{EmissionState, Spot, SpotType};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::sync::watch;

/// Every `Id` this output sends starts with this; `[[wsjtx_relay]]`
/// ignores datagrams that carry it (D14).
pub const ID_PREFIX: &str = "manta-";

/// The `NetworkMessage` schema manta writes (and its heartbeat's maximum).
pub const SCHEMA: u32 = 3;
/// The heartbeat (and status refresh) period, WSJT-X's own `pulse`.
pub const PULSE: Duration = Duration::from_secs(15);
/// The `NetworkMessage` magic number every datagram starts with.
const MAGIC: u32 = 0xadbc_cbda;
/// `Status`'s frequency tolerance and T/R period: not applicable.
const NOT_APPLICABLE: u32 = 0xffff_ffff;
/// `Status`'s configuration name.
const CONFIGURATION_NAME: &str = "manta";

const HEARTBEAT: u32 = 0;
const STATUS: u32 = 1;
const DECODE: u32 = 2;
const CLOSE: u32 = 6;

/// Why a spot was not sent; the `reason` label of
/// `manta_wsjtx_output_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsjtxOutputDrop {
    /// The worker fell behind the spot bus.
    Lagged,
    /// The spot's `Status` or `Decode` failed to send.
    SendError,
}

impl WsjtxOutputDrop {
    /// Every reason, in render order.
    pub const ALL: [WsjtxOutputDrop; 2] = [WsjtxOutputDrop::Lagged, WsjtxOutputDrop::SendError];

    /// The `reason` label value.
    pub fn label(self) -> &'static str {
        match self {
            WsjtxOutputDrop::Lagged => "lagged",
            WsjtxOutputDrop::SendError => "send_error",
        }
    }
}

/// A big-endian `QDataStream` writer.
struct Writer(Vec<u8>);

impl Writer {
    fn message(kind: u32, id: &str) -> Self {
        let mut w = Writer(Vec::with_capacity(128));
        w.u32(MAGIC).u32(SCHEMA).u32(kind).utf8(id);
        w
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    /// A `QByteArray` of UTF-8: a 32-bit length, then the bytes. An empty
    /// string is written as length 0, never as the null marker.
    fn utf8(&mut self, s: &str) -> &mut Self {
        let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.0.extend_from_slice(&s.as_bytes()[..len as usize]);
        self
    }
}

/// The fields of a `Status` (type 1) that vary; the rest are fixed (D12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFields {
    pub id: String,
    pub dial_freq_hz: u64,
    /// `CW` or `RTTY`; also written as the Tx mode.
    pub mode: String,
    /// The station call without its SSID.
    pub de_call: String,
    /// The `[server]` grid, or empty.
    pub de_grid: String,
}

/// A `Decode` (type 2).
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeFields {
    pub id: String,
    pub new: bool,
    /// Milliseconds since midnight UTC.
    pub time_ms: u32,
    /// The 2500 Hz-referenced SNR, rounded (D11).
    pub snr: i32,
    pub delta_time_s: f64,
    /// The spot frequency minus the `Status` dial, 0 to 999 Hz.
    pub delta_freq_hz: u32,
    pub mode: String,
    /// `CQ <call>` or `DE <call>`.
    pub message: String,
    pub low_confidence: bool,
    /// True on a file replay: "decodes that result from playing back a
    /// .WAV file" in WSJT-X's terms (D10).
    pub off_air: bool,
}

/// A `Heartbeat` (type 0): maximum schema [`SCHEMA`], then the version and
/// revision.
pub fn heartbeat(id: &str, version: &str, revision: &str) -> Vec<u8> {
    let mut w = Writer::message(HEARTBEAT, id);
    w.u32(SCHEMA).utf8(version).utf8(revision);
    w.0
}

/// A `Status` (type 1) in the complete master-schema layout, through
/// `Tx Message`, so a reader of any schema-3 version finds every field.
pub fn status(s: &StatusFields) -> Vec<u8> {
    let mut w = Writer::message(STATUS, &s.id);
    w.u64(s.dial_freq_hz)
        .utf8(&s.mode) // Mode
        .utf8("") // DX call
        .utf8("") // Report
        .utf8(&s.mode) // Tx Mode
        .bool(false) // Tx Enabled
        .bool(false) // Transmitting
        .bool(false) // Decoding
        .u32(0) // Rx DF
        .u32(0) // Tx DF
        .utf8(&s.de_call)
        .utf8(&s.de_grid)
        .utf8("") // DX grid
        .bool(false) // Tx Watchdog
        .utf8("") // Sub-mode
        .bool(false) // Fast mode
        .u8(0) // Special Operation Mode
        .u32(NOT_APPLICABLE) // Frequency Tolerance
        .u32(NOT_APPLICABLE) // T/R Period
        .utf8(CONFIGURATION_NAME)
        .utf8(""); // Tx Message
    w.0
}

/// A `Decode` (type 2).
pub fn decode(d: &DecodeFields) -> Vec<u8> {
    let mut w = Writer::message(DECODE, &d.id);
    w.bool(d.new)
        .u32(d.time_ms)
        .i32(d.snr)
        .f64(d.delta_time_s)
        .u32(d.delta_freq_hz)
        .utf8(&d.mode)
        .utf8(&d.message)
        .bool(d.low_confidence)
        .bool(d.off_air);
    w.0
}

/// A `Close` (type 6).
pub fn close(id: &str) -> Vec<u8> {
    Writer::message(CLOSE, id).0
}

/// The instance `Id` for a frequency: `manta-<band>`, `manta-unknown`
/// outside the amateur bands.
pub fn instance_id(freq_hz: f64) -> String {
    format!("{ID_PREFIX}{}", band_for_freq_hz(freq_hz))
}

/// Maps a spot to its instance `Id`, the `Status` dial (the frequency in
/// whole Hz, rounded down to a kHz) and its `Decode` (D12).
pub fn spot_fields(spot: &Spot, unix_ts: i64, off_air: bool) -> (String, u64, DecodeFields) {
    let rf_hz = spot.freq_hz.round().max(0.0) as u64;
    let dial = rf_hz - rf_hz % 1000;
    let id = instance_id(spot.freq_hz);
    let time_ms = (unix_ts.rem_euclid(86_400) * 1000) as u32;
    let verb = if spot.spot_type == SpotType::Cq {
        "CQ"
    } else {
        "DE"
    };
    let fields = DecodeFields {
        id: id.clone(),
        new: true,
        time_ms,
        snr: spot.snr_db.round() as i32,
        delta_time_s: 0.0,
        delta_freq_hz: (rf_hz - dial) as u32,
        mode: spot.mode.label().to_string(),
        message: format!("{verb} {}", spot.callsign),
        low_confidence: spot.emission_state == EmissionState::Tentative,
        off_air,
    };
    (id, dial, fields)
}

/// One `[[wsjtx_output]]` block, as the worker needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct WsjtxOutput {
    /// The log prefix and `destination` label.
    pub label: String,
    pub destination: std::net::SocketAddr,
    /// The station call without its SSID.
    pub de_call: String,
    /// The `[server]` grid, or empty.
    pub de_grid: String,
    /// The heartbeat's version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The heartbeat's revision (the build's git SHA).
    pub revision: String,
    /// True on a file replay (D10).
    pub off_air: bool,
}

/// What the last `Status` for one `Id` said.
struct IdState {
    dial_freq_hz: u64,
    mode: String,
}

struct Output {
    cfg: WsjtxOutput,
    socket: tokio::net::UdpSocket,
    bus: Arc<SpotBus>,
    stats: Arc<WsjtxOutputStats>,
    /// Every `Id` used so far, with its last `Status`; ordered, so
    /// heartbeats and `Close`s go out in a stable order.
    ids: BTreeMap<String, Option<IdState>>,
    warned_send: bool,
}

impl Output {
    /// Hands one datagram to the OS; false (warned once) on failure.
    async fn send(&mut self, datagram: &[u8]) -> bool {
        match self.socket.send_to(datagram, self.cfg.destination).await {
            Ok(_) => {
                self.stats.record_datagram();
                true
            }
            Err(e) => {
                if !self.warned_send {
                    self.warned_send = true;
                    tracing::warn!(
                        "wsjtx_output {}: cannot send: {e}; manta keeps trying and counts each \
                         lost spot in manta_wsjtx_output_dropped_total{{reason=\"send_error\"}}",
                        self.cfg.label
                    );
                }
                false
            }
        }
    }

    fn status_fields(&self, id: &str, dial_freq_hz: u64, mode: &str) -> StatusFields {
        StatusFields {
            id: id.to_string(),
            dial_freq_hz,
            mode: mode.to_string(),
            de_call: self.cfg.de_call.clone(),
            de_grid: self.cfg.de_grid.clone(),
        }
    }

    /// Sends one spot: a `Heartbeat` on an `Id`'s first use, a `Status`
    /// when the dial or mode changed, then the `Decode`.
    async fn spot(&mut self, bus_spot: &BusSpot) {
        let spot = &bus_spot.spot;
        let unix_ts = self.bus.unix_ts_for(spot.sample_ts);
        let (id, dial, fields) = spot_fields(spot, unix_ts, self.cfg.off_air);
        if !self.ids.contains_key(&id) {
            self.ids.insert(id.clone(), None);
            let hb = heartbeat(&id, &self.cfg.version, &self.cfg.revision);
            self.send(&hb).await;
        }
        let stale = !matches!(
            self.ids.get(&id),
            Some(Some(s)) if s.dial_freq_hz == dial && s.mode == fields.mode
        );
        if stale {
            let st = status(&self.status_fields(&id, dial, &fields.mode));
            if !self.send(&st).await {
                self.stats.record_dropped(WsjtxOutputDrop::SendError, 1);
                return;
            }
            self.ids.insert(
                id.clone(),
                Some(IdState {
                    dial_freq_hz: dial,
                    mode: fields.mode.clone(),
                }),
            );
        }
        if self.send(&decode(&fields)).await {
            self.stats.record_spot();
        } else {
            self.stats.record_dropped(WsjtxOutputDrop::SendError, 1);
        }
    }

    /// Every [`PULSE`]: a `Heartbeat`, then the last `Status`, for every
    /// `Id` used so far.
    async fn pulse(&mut self) {
        let ids: Vec<(String, Option<(u64, String)>)> = self
            .ids
            .iter()
            .map(|(id, s)| {
                (
                    id.clone(),
                    s.as_ref().map(|s| (s.dial_freq_hz, s.mode.clone())),
                )
            })
            .collect();
        for (id, last) in ids {
            let hb = heartbeat(&id, &self.cfg.version, &self.cfg.revision);
            self.send(&hb).await;
            if let Some((dial, mode)) = last {
                let st = status(&self.status_fields(&id, dial, &mode));
                self.send(&st).await;
            }
        }
    }

    /// Shutdown: sends the spots still queued, then a `Close` per `Id`.
    async fn finish(&mut self, rx: &mut broadcast::Receiver<BusSpot>) {
        loop {
            match rx.try_recv() {
                Ok(spot) => self.spot(&spot).await,
                Err(TryRecvError::Lagged(n)) => {
                    self.stats.record_dropped(WsjtxOutputDrop::Lagged, n)
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let ids: Vec<String> = self.ids.keys().cloned().collect();
        for id in ids {
            self.send(&close(&id)).await;
        }
    }
}

/// Runs one WSJT-X output over a pre-bound (nonblocking) socket until
/// shutdown, then sends what is queued and a `Close` per `Id`.
/// `rx` must be subscribed before the task is spawned, so no spot
/// published while it starts is missed.
pub async fn run(
    cfg: WsjtxOutput,
    socket: std::net::UdpSocket,
    bus: Arc<SpotBus>,
    mut rx: broadcast::Receiver<BusSpot>,
    stats: Arc<WsjtxOutputStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let socket = match tokio::net::UdpSocket::from_std(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("wsjtx_output {}: cannot use the socket: {e}", cfg.label);
            return;
        }
    };
    let mut out = Output {
        cfg,
        socket,
        bus,
        stats,
        ids: BTreeMap::new(),
        warned_send: false,
    };
    let mut pulse = tokio::time::interval_at(tokio::time::Instant::now() + PULSE, PULSE);
    pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if *shutdown.borrow() {
            out.finish(&mut rx).await;
            return;
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    out.finish(&mut rx).await;
                    return;
                }
            }
            _ = pulse.tick() => out.pulse().await,
            received = rx.recv() => match received {
                Ok(spot) => out.spot(&spot).await,
                Err(RecvError::Lagged(n)) => {
                    out.stats.record_dropped(WsjtxOutputDrop::Lagged, n);
                }
                Err(RecvError::Closed) => {
                    out.finish(&mut rx).await;
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::wsjtx_proto::{self, Message};
    use manta_spot::SpotMode;
    use std::time::SystemTime;

    /// 2026-11-03T20:07:01Z, the design spot's time.
    const SPOT_UNIX: i64 = 1_793_650_021;
    const EPOCH_UNIX: u64 = 1_793_650_000;
    const WAIT: Duration = Duration::from_secs(5);

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    fn spot(call: &str, freq_hz: f64, spot_type: SpotType) -> Spot {
        Spot {
            callsign: call.to_string(),
            freq_hz,
            snr_db: 21.0,
            wpm: 20.0,
            spot_type,
            confidence: 0.64,
            emission_state: EmissionState::Confirmed,
            track_id: 1,
            // One sample per second on the test bus: 21 s after its epoch.
            sample_ts: (SPOT_UNIX - EPOCH_UNIX as i64) as u64,
            rst: None,
            qrl_query: false,
            mode: SpotMode::Cw,
            baud: None,
        }
    }

    fn design_status() -> StatusFields {
        StatusFields {
            id: "manta-20m".to_string(),
            dial_freq_hz: 14_012_000,
            mode: "CW".to_string(),
            de_call: "W5AU".to_string(),
            de_grid: "EM10dh".to_string(),
        }
    }

    fn kind(datagram: &[u8]) -> u32 {
        u32::from_be_bytes(datagram[8..12].try_into().unwrap())
    }

    #[test]
    fn heartbeat_matches_the_design_bytes() {
        let want = hex(
            "ad bc cb da 00 00 00 03 00 00 00 00 00 00 00 09 6d 61 6e 74 61 2d 32 30 6d 00 00 00 03 \
             00 00 00 05 30 2e 31 2e 30 00 00 00 0c 64 31 64 64 66 39 39 61 33 36 63 65",
        );
        assert_eq!(want.len(), 54);
        assert_eq!(heartbeat("manta-20m", "0.1.0", "d1ddf99a36ce"), want);
    }

    #[test]
    fn decode_matches_the_design_bytes() {
        let want = hex(
            "ad bc cb da 00 00 00 03 00 00 00 02 00 00 00 09 6d 61 6e 74 61 2d 32 30 6d 01 04 51 0e 88 \
             00 00 00 15 00 00 00 00 00 00 00 00 00 00 01 54 00 00 00 02 43 57 00 00 00 07 43 51 20 \
             57 31 41 57 00 01",
        );
        assert_eq!(want.len(), 65);
        let (id, dial, fields) =
            spot_fields(&spot("W1AW", 14_012_340.0, SpotType::Cq), SPOT_UNIX, true);
        assert_eq!((id.as_str(), dial), ("manta-20m", 14_012_000));
        assert_eq!(decode(&fields), want);
    }

    #[test]
    fn every_message_parses_with_the_relay_reader() {
        assert_eq!(
            wsjtx_proto::parse(&heartbeat("manta-20m", "0.1.0", "abc")).unwrap(),
            Message::Other {
                kind: 0,
                id: "manta-20m".to_string()
            }
        );
        assert_eq!(
            wsjtx_proto::parse(&close("manta-40m")).unwrap(),
            Message::Other {
                kind: 6,
                id: "manta-40m".to_string()
            }
        );
        match wsjtx_proto::parse(&status(&design_status())).unwrap() {
            Message::Status(s) => {
                assert_eq!(s.id, "manta-20m");
                assert_eq!(s.dial_freq_hz, 14_012_000);
                assert_eq!(s.mode, "CW");
            }
            other => panic!("not a Status: {other:?}"),
        }
        let (_, _, fields) =
            spot_fields(&spot("W1AW", 14_012_340.0, SpotType::Cq), SPOT_UNIX, false);
        match wsjtx_proto::parse(&decode(&fields)).unwrap() {
            Message::Decode(d) => {
                assert_eq!(d.id, fields.id);
                assert_eq!(d.new, fields.new);
                assert_eq!(d.time_ms, fields.time_ms);
                assert_eq!(d.snr, fields.snr);
                assert_eq!(d.delta_time_s, fields.delta_time_s);
                assert_eq!(d.delta_freq_hz, fields.delta_freq_hz);
                assert_eq!(d.mode, fields.mode);
                assert_eq!(d.message, fields.message);
                assert_eq!(d.low_confidence, fields.low_confidence);
                assert_eq!(d.off_air, fields.off_air);
            }
            other => panic!("not a Decode: {other:?}"),
        }
    }

    #[test]
    fn status_writes_the_complete_master_layout() {
        let bytes = status(&design_status());
        assert_eq!(bytes.len(), 114);
        assert_eq!(kind(&bytes), 1);
        // ... Special Operation Mode 0, tolerance and T/R period not
        // applicable, configuration name `manta`, then an empty Tx Message.
        let tail = hex("00 ff ff ff ff ff ff ff ff 00 00 00 05 6d 61 6e 74 61 00 00 00 00");
        assert!(bytes.ends_with(&tail), "{bytes:02x?}");
    }

    #[test]
    fn a_spot_maps_to_its_band_id_dial_and_offset() {
        let (id, dial, f) =
            spot_fields(&spot("W1AW", 14_012_340.0, SpotType::Cq), SPOT_UNIX, false);
        assert_eq!(
            (id.as_str(), dial, f.delta_freq_hz),
            ("manta-20m", 14_012_000, 340)
        );
        let (id, dial, f) = spot_fields(&spot("W1AW", 7_000_999.6, SpotType::Cq), SPOT_UNIX, false);
        assert_eq!(
            (id.as_str(), dial, f.delta_freq_hz),
            ("manta-40m", 7_001_000, 0)
        );
        let (id, _, _) = spot_fields(&spot("W1AW", 10_000_000.0, SpotType::Cq), SPOT_UNIX, false);
        assert_eq!(id, "manta-unknown");
    }

    #[test]
    fn message_text_follows_the_spot_type() {
        let text = |t| {
            spot_fields(&spot("W1AW", 14_012_340.0, t), SPOT_UNIX, false)
                .2
                .message
        };
        assert_eq!(text(SpotType::Cq), "CQ W1AW");
        assert_eq!(text(SpotType::De), "DE W1AW");
        assert_eq!(text(SpotType::Beacon), "DE W1AW");
        assert_eq!(text(SpotType::Unknown), "DE W1AW");
    }

    #[test]
    fn tentative_spots_are_low_confidence() {
        let mut s = spot("W1AW", 14_012_340.0, SpotType::Cq);
        assert!(!spot_fields(&s, SPOT_UNIX, false).2.low_confidence);
        s.emission_state = EmissionState::Tentative;
        assert!(spot_fields(&s, SPOT_UNIX, false).2.low_confidence);
    }

    #[test]
    fn time_is_the_spots_utc_time_of_day_in_ms() {
        let f = spot_fields(&spot("W1AW", 14_012_340.0, SpotType::Cq), SPOT_UNIX, false).2;
        // 20:07:01 UTC.
        assert_eq!(f.time_ms, 72_421_000);
        assert_eq!(f.time_ms, (20 * 3600 + 7 * 60 + 1) * 1000);
    }

    // ---- the worker, against a fake logger on loopback

    struct Harness {
        logger: tokio::net::UdpSocket,
        tx: broadcast::Sender<BusSpot>,
        shutdown: watch::Sender<bool>,
        stats: Arc<WsjtxOutputStats>,
        task: tokio::task::JoinHandle<()>,
    }

    fn test_bus() -> Arc<SpotBus> {
        Arc::new(SpotBus::new(
            1.0,
            SystemTime::UNIX_EPOCH + Duration::from_secs(EPOCH_UNIX),
            1,
        ))
    }

    fn output_socket() -> std::net::UdpSocket {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        sock
    }

    fn cfg(destination: std::net::SocketAddr, off_air: bool) -> WsjtxOutput {
        WsjtxOutput {
            label: destination.to_string(),
            destination,
            de_call: "W5AU".to_string(),
            de_grid: "EM10dh".to_string(),
            version: "0.1.0".to_string(),
            revision: "d1ddf99a36ce".to_string(),
            off_air,
        }
    }

    async fn start(off_air: bool) -> Harness {
        let logger = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dest = logger.local_addr().unwrap();
        let (tx, rx) = broadcast::channel(64);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let stats = Arc::new(WsjtxOutputStats::new(dest.to_string()));
        let task = tokio::spawn(run(
            cfg(dest, off_air),
            output_socket(),
            test_bus(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        Harness {
            logger,
            tx,
            shutdown,
            stats,
            task,
        }
    }

    fn publish(h: &Harness, s: Spot) {
        h.tx.send(BusSpot {
            spot: s,
            occurrence_count: 1,
        })
        .unwrap();
    }

    /// Every datagram that arrives before 200 ms of silence.
    ///
    /// MAN-478: the tests that check what the fake logger reads run in
    /// real time. On a paused clock the 200 ms could auto-advance before
    /// kqueue reported a datagram already sent, and macOS then read no
    /// datagrams at all. They pause the clock only across a 15 s
    /// [`PULSE`] wait, when nothing reads the socket.
    async fn drain(logger: &tokio::net::UdpSocket) -> Vec<Vec<u8>> {
        let mut got = Vec::new();
        let mut buf = vec![0u8; 2048];
        while let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(200), logger.recv_from(&mut buf)).await
        {
            got.push(buf[..n].to_vec());
        }
        got
    }

    /// `(type, Id)` of each datagram, read with the relay's reader.
    fn summary(datagrams: &[Vec<u8>]) -> Vec<(u32, String)> {
        datagrams
            .iter()
            .map(|d| (kind(d), wsjtx_proto::parse(d).unwrap().id().to_string()))
            .collect()
    }

    fn ids(pairs: &[(u32, &str)]) -> Vec<(u32, String)> {
        pairs.iter().map(|(k, id)| (*k, id.to_string())).collect()
    }

    /// One [`PULSE`] on a paused clock; what it sends waits in the
    /// logger's buffer for the next [`drain`].
    async fn wait_a_pulse() {
        tokio::time::pause();
        tokio::time::sleep(PULSE).await;
        tokio::time::resume();
    }

    #[tokio::test]
    async fn a_first_spot_on_a_band_sends_heartbeat_status_then_decode() {
        let h = start(false).await;
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        let got = drain(&h.logger).await;
        assert_eq!(
            summary(&got),
            ids(&[(0, "manta-20m"), (1, "manta-20m"), (2, "manta-20m")])
        );
        assert_eq!(got[0], heartbeat("manta-20m", "0.1.0", "d1ddf99a36ce"));
        assert_eq!(got[1], status(&design_status()));
        match wsjtx_proto::parse(&got[2]).unwrap() {
            Message::Decode(d) => {
                assert!(d.new);
                assert!(!d.off_air, "a live source");
                assert!(!d.low_confidence);
                assert_eq!(d.message, "CQ W1AW");
                assert_eq!(d.delta_freq_hz, 340);
                assert_eq!(d.snr, 21);
                assert_eq!(d.time_ms, 72_421_000);
                assert_eq!(d.mode, "CW");
            }
            other => panic!("not a Decode: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_spot_on_the_same_khz_sends_only_a_decode() {
        let h = start(false).await;
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        assert_eq!(drain(&h.logger).await.len(), 3);
        publish(&h, spot("K5ARH", 14_012_870.0, SpotType::De));
        let got = drain(&h.logger).await;
        assert_eq!(summary(&got), ids(&[(2, "manta-20m")]));
    }

    #[tokio::test]
    async fn a_spot_on_another_khz_sends_a_status_first() {
        let h = start(false).await;
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        assert_eq!(drain(&h.logger).await.len(), 3);
        publish(&h, spot("K5ARH", 14_025_100.0, SpotType::Cq));
        let got = drain(&h.logger).await;
        assert_eq!(summary(&got), ids(&[(1, "manta-20m"), (2, "manta-20m")]));
        match wsjtx_proto::parse(&got[0]).unwrap() {
            Message::Status(s) => assert_eq!(s.dial_freq_hz, 14_025_000),
            other => panic!("not a Status: {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_bands_use_two_ids() {
        let h = start(false).await;
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        publish(&h, spot("K5ARH", 7_024_000.0, SpotType::Cq));
        let got = drain(&h.logger).await;
        assert_eq!(
            summary(&got),
            ids(&[
                (0, "manta-20m"),
                (1, "manta-20m"),
                (2, "manta-20m"),
                (0, "manta-40m"),
                (1, "manta-40m"),
                (2, "manta-40m"),
            ])
        );
    }

    #[tokio::test]
    async fn every_15_s_each_id_gets_a_heartbeat_and_a_status() {
        let h = start(false).await;
        assert!(drain(&h.logger).await.is_empty(), "no Id, no heartbeat");
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        publish(&h, spot("K5ARH", 7_024_000.0, SpotType::Cq));
        assert_eq!(drain(&h.logger).await.len(), 6);
        wait_a_pulse().await;
        let got = drain(&h.logger).await;
        assert_eq!(
            summary(&got),
            ids(&[
                (0, "manta-20m"),
                (1, "manta-20m"),
                (0, "manta-40m"),
                (1, "manta-40m"),
            ])
        );
        wait_a_pulse().await;
        assert_eq!(drain(&h.logger).await.len(), 4, "and again 15 s later");
    }

    #[tokio::test]
    async fn shutdown_sends_queued_spots_then_a_close_per_id() {
        let h = start(true).await;
        // Queued before the worker first runs; shutdown is already set.
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        publish(&h, spot("K5ARH", 7_024_000.0, SpotType::Cq));
        h.shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), h.task)
            .await
            .expect("the worker ends at shutdown")
            .unwrap();
        let got = drain(&h.logger).await;
        assert_eq!(
            summary(&got),
            ids(&[
                (0, "manta-20m"),
                (1, "manta-20m"),
                (2, "manta-20m"),
                (0, "manta-40m"),
                (1, "manta-40m"),
                (2, "manta-40m"),
                (6, "manta-20m"),
                (6, "manta-40m"),
            ])
        );
        match wsjtx_proto::parse(&got[2]).unwrap() {
            Message::Decode(d) => assert!(d.off_air, "a replay's decodes are off air"),
            other => panic!("not a Decode: {other:?}"),
        }
        assert_eq!(h.stats.spots(), 2);
    }

    #[tokio::test]
    async fn only_types_0_1_2_and_6_are_ever_sent() {
        let h = start(false).await;
        let mut all = Vec::new();
        for (call, f, t) in [
            ("W1AW", 14_012_340.0, SpotType::Cq),
            ("K5ARH", 14_031_000.0, SpotType::De),
            ("N1DQ", 7_024_000.0, SpotType::Beacon),
            ("KB1MBX", 10_000_000.0, SpotType::Unknown),
        ] {
            publish(&h, spot(call, f, t));
            all.extend(drain(&h.logger).await);
        }
        wait_a_pulse().await;
        all.extend(drain(&h.logger).await);
        h.shutdown.send(true).unwrap();
        h.task.await.unwrap();
        all.extend(drain(&h.logger).await);
        let kinds: std::collections::BTreeSet<u32> = all.iter().map(|d| kind(d)).collect();
        assert_eq!(kinds, [0, 1, 2, 6].into_iter().collect());
        assert!(all.iter().all(|d| kind(d) != 5 && kind(d) != 12));
    }

    #[tokio::test]
    async fn the_relay_ignores_this_output() {
        use crate::metrics::{DropReason, Metrics, RelayKind};
        use crate::relay::wsjtx::{self, WsjtxRelay};
        let relay_socket = output_socket();
        let relay_addr = relay_socket.local_addr().unwrap();
        let metrics = Metrics::default();
        let relay_stats = metrics.register_relay(relay_addr.to_string(), RelayKind::Wsjtx);
        let relay_bus = test_bus();
        let mut relay_rx = relay_bus.subscribe_relay();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let relay = tokio::spawn(wsjtx::run(
            WsjtxRelay {
                label: relay_addr.to_string(),
                callsign: "W1AW".to_string(),
            },
            relay_socket,
            Arc::new(|| SPOT_UNIX),
            relay_bus,
            relay_stats.clone(),
            shutdown_rx.clone(),
        ));
        let (tx, rx) = broadcast::channel(8);
        let stats = Arc::new(WsjtxOutputStats::new(relay_addr.to_string()));
        let output = tokio::spawn(run(
            cfg(relay_addr, false),
            output_socket(),
            test_bus(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        tx.send(BusSpot {
            spot: spot("W1AW", 14_012_340.0, SpotType::Cq),
            occurrence_count: 1,
        })
        .unwrap();
        tokio::time::timeout(WAIT, async {
            while relay_stats.dropped_total(DropReason::OwnOutput) < 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the relay hears all three datagrams");
        assert_eq!(stats.datagrams(), 3);
        assert_eq!(relay_stats.dropped_total(DropReason::OwnOutput), 3);
        assert!(
            !relay_stats.up(),
            "manta's own output never marks the relay up"
        );
        assert!(relay_rx.try_recv().is_err(), "no relayed spot");
        shutdown.send(true).unwrap();
        output.await.unwrap();
        relay.await.unwrap();
    }

    #[tokio::test]
    async fn the_counters_match_what_was_sent() {
        let h = start(false).await;
        publish(&h, spot("W1AW", 14_012_340.0, SpotType::Cq));
        publish(&h, spot("K5ARH", 14_012_870.0, SpotType::De));
        let mut n = drain(&h.logger).await.len();
        h.shutdown.send(true).unwrap();
        h.task.await.unwrap();
        n += drain(&h.logger).await.len();
        // HB, Status, Decode, Decode, then Close.
        assert_eq!(n, 5);
        assert_eq!(h.stats.datagrams(), 5);
        assert_eq!(h.stats.spots(), 2);
        for reason in WsjtxOutputDrop::ALL {
            assert_eq!(h.stats.dropped(reason), 0, "{}", reason.label());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lagging_worker_counts_the_spots_it_lost() {
        let logger = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dest = logger.local_addr().unwrap();
        let (tx, rx) = broadcast::channel(2);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let stats = Arc::new(WsjtxOutputStats::new(dest.to_string()));
        // Five spots into a channel of two before the worker first runs.
        for _ in 0..5 {
            tx.send(BusSpot {
                spot: spot("W1AW", 14_012_340.0, SpotType::Cq),
                occurrence_count: 1,
            })
            .unwrap();
        }
        let task = tokio::spawn(run(
            cfg(dest, false),
            output_socket(),
            test_bus(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        drain(&logger).await;
        shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(stats.dropped(WsjtxOutputDrop::Lagged), 3);
        assert_eq!(stats.spots(), 2);
    }

    /// Without `SO_BROADCAST`, Linux refuses a send to the loopback
    /// broadcast address locally, so nothing leaves the host.
    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn a_failed_send_counts_send_error_and_the_worker_keeps_going() {
        let dest: std::net::SocketAddr = "127.255.255.255:9".parse().unwrap();
        let (tx, rx) = broadcast::channel(8);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let stats = Arc::new(WsjtxOutputStats::new(dest.to_string()));
        let task = tokio::spawn(run(
            cfg(dest, false),
            output_socket(),
            test_bus(),
            rx,
            stats.clone(),
            shutdown_rx,
        ));
        for _ in 0..2 {
            tx.send(BusSpot {
                spot: spot("W1AW", 14_012_340.0, SpotType::Cq),
                occurrence_count: 1,
            })
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(stats.dropped(WsjtxOutputDrop::SendError), 2);
        assert_eq!(stats.spots(), 0);
        assert_eq!(stats.datagrams(), 0);
    }
}
