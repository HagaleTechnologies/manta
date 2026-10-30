//! MAN-320: the WSJT-X relay worker. It listens on a pre-bound UDP socket
//! for WSJT-X's `NetworkMessage` datagrams ([`super::wsjtx_proto`]) and
//! forwards each new FT8/FT4 `CQ` decode to the RBN uplink as a digital
//! spot under the block's `callsign`.
//!
//! D7's gate chain, in order, each drop counted by its
//! `manta_relay_dropped_total` reason: `bad_datagram` (does not decode) ->
//! `replay` (`New` false) -> `off_air` (`.wav` playback) ->
//! `low_confidence` -> `no_dial` (no `Status` from that instance `Id` yet,
//! or dial 0) -> `mode` (the instance's latest `Status` mode is not `FT8`
//! or `FT4`; the mode name comes from `Status`, never from `Decode`'s mode
//! field) -> `not_cq` -> `no_call` (no caller, or a hashed `<...>` call) ->
//! `duplicate` (same call, mode and band within 600 s, the window manta's
//! own dedupe uses). State is per instance `Id`, bounded to
//! [`MAX_INSTANCES`] (the oldest-heard is evicted).
//!
//! The spot's frequency is the instance's dial plus the decode's audio
//! offset; its SNR is WSJT-X's own 2500 Hz figure, forwarded verbatim
//! (D8); its time is arrival time from the injected clock (D9).
//! `manta_relay_up` is 1 while any WSJT-X datagram (heartbeats included)
//! arrived within [`UP_WINDOW`]. See
//! `docs/DECISIONS/2026-10-27-man320-relay-local-skimmers-and-wsjtx.md`.

use super::wsjtx_proto::{self, Decode, Message, Status};
use super::{plausible_relayed_call, RelayMode, RelaySpot};
use crate::band::band_for_freq_hz;
use crate::bus::SpotBus;
use crate::metrics::{DropReason, RelayStats};
use crate::rbn_compare::Clock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// D7: a call, mode and band forwarded within this many seconds is a
/// duplicate; manta-spot's `dedupe.rs` suppression window.
pub const DUPLICATE_WINDOW_SECS: i64 = 600;
/// D7: WSJT-X instances tracked per listener; the oldest-heard is evicted.
pub const MAX_INSTANCES: usize = 64;
/// The dedupe map's bound, as `bus.rs`'s occurrence map is bounded.
pub const MAX_DEDUPE_ENTRIES: usize = 20_000;
/// `manta_relay_up` drops to 0 after this long without a datagram.
pub const UP_WINDOW: Duration = Duration::from_secs(60);

/// One `[[wsjtx_relay]]` block, as the worker needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct WsjtxRelay {
    /// The `relay` label and log prefix: the bound address.
    pub label: String,
    /// The spotter call every forwarded spot goes out under.
    pub callsign: String,
}

/// The caller a decode's text names, if it is a `CQ` (D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CqCaller {
    /// Not a `CQ` message.
    NotCq,
    /// A `CQ` with no usable caller: none, or a hashed `<...>` call.
    NoCall,
    /// The caller, uppercase, passing [`plausible_relayed_call`].
    Call(String),
}

/// Reads the caller from an FT8/FT4 message: `CQ [modifier] CALL [GRID]`.
/// The modifier (`DX`, `POTA`, `NA`, `290` ...) is one token of 1-4
/// letters or 1-3 digits that is not itself a plausible call; the next
/// token, with `<>` stripped, must pass [`plausible_relayed_call`].
pub fn cq_caller(message: &str) -> CqCaller {
    let mut tokens = message.split_ascii_whitespace();
    if !tokens.next().is_some_and(|t| t.eq_ignore_ascii_case("CQ")) {
        return CqCaller::NotCq;
    }
    let Some(mut token) = tokens.next() else {
        return CqCaller::NoCall;
    };
    if is_modifier(token) {
        let Some(next) = tokens.next() else {
            return CqCaller::NoCall;
        };
        token = next;
    }
    let call = token
        .strip_prefix('<')
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(token)
        .to_ascii_uppercase();
    if plausible_relayed_call(&call) {
        CqCaller::Call(call)
    } else {
        CqCaller::NoCall
    }
}

fn is_modifier(token: &str) -> bool {
    let letters = (1..=4).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphabetic());
    let digits = (1..=3).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_digit());
    (letters || digits) && !plausible_relayed_call(&token.to_ascii_uppercase())
}

/// D7's 600 s dedupe over (call, mode, band), bounded: a full map prunes
/// expired entries first, then evicts the oldest.
#[derive(Debug)]
pub struct Dedupe {
    last: HashMap<(String, RelayMode, &'static str), i64>,
    capacity: usize,
}

impl Default for Dedupe {
    fn default() -> Self {
        Self::with_capacity(MAX_DEDUPE_ENTRIES)
    }
}

impl Dedupe {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            last: HashMap::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }

    /// True when the spot is fresh, and records it as forwarded at `now`;
    /// false for a duplicate of one forwarded within the window (which
    /// does not restart the window).
    pub fn admit(&mut self, call: &str, mode: RelayMode, band: &'static str, now: i64) -> bool {
        let key = (call.to_string(), mode, band);
        if let Some(&at) = self.last.get(&key) {
            if now - at < DUPLICATE_WINDOW_SECS {
                return false;
            }
        } else if self.last.len() >= self.capacity {
            self.last.retain(|_, at| now - *at < DUPLICATE_WINDOW_SECS);
            if self.last.len() >= self.capacity {
                if let Some(oldest) = self
                    .last
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone())
                {
                    self.last.remove(&oldest);
                }
            }
        }
        self.last.insert(key, now);
        true
    }

    /// Withdraws an admission whose spot reached no uplink (none was
    /// connected), so the next CQ from that call is not a duplicate of a
    /// spot that was never sent.
    pub fn forget(&mut self, call: &str, mode: RelayMode, band: &'static str) {
        self.last.remove(&(call.to_string(), mode, band));
    }
}

#[derive(Debug)]
struct Instance {
    dial_freq_hz: u64,
    mode: String,
    /// Arrival order of this instance's latest datagram.
    heard: u64,
}

/// The per-listener state the gate chain runs over.
struct Relay {
    cfg: WsjtxRelay,
    spotter: Arc<str>,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    instances: HashMap<String, Instance>,
    seq: u64,
    dedupe: Dedupe,
}

impl Relay {
    /// Runs one datagram through the gate chain; true if it decoded.
    fn handle(&mut self, datagram: &[u8]) -> bool {
        let msg = match wsjtx_proto::parse(datagram) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::debug!("wsjtx_relay {}: dropping a datagram: {e}", self.cfg.label);
                self.stats.record_dropped(DropReason::BadDatagram);
                return false;
            }
        };
        self.seq += 1;
        match msg {
            Message::Status(s) => self.status(s),
            Message::Decode(d) => self.decode(d),
            Message::Other(_) => {}
        }
        true
    }

    fn status(&mut self, s: Status) {
        if let Some(inst) = self.instances.get_mut(&s.id) {
            inst.dial_freq_hz = s.dial_freq_hz;
            inst.mode = s.mode;
            inst.heard = self.seq;
            return;
        }
        if self.instances.len() >= MAX_INSTANCES {
            if let Some(oldest) = self
                .instances
                .iter()
                .min_by_key(|(_, i)| i.heard)
                .map(|(id, _)| id.clone())
            {
                self.instances.remove(&oldest);
            }
        }
        tracing::info!(
            "wsjtx_relay {}: hearing WSJT-X instance \"{}\" ({}, dial {} Hz)",
            self.cfg.label,
            s.id,
            s.mode,
            s.dial_freq_hz
        );
        self.instances.insert(
            s.id,
            Instance {
                dial_freq_hz: s.dial_freq_hz,
                mode: s.mode,
                heard: self.seq,
            },
        );
    }

    fn decode(&mut self, d: Decode) {
        if let Some(inst) = self.instances.get_mut(&d.id) {
            inst.heard = self.seq;
        }
        match self.gate(&d) {
            Ok(spot) => {
                let key = (
                    spot.callsign.clone(),
                    spot.mode,
                    band_for_freq_hz(spot.freq_hz),
                );
                // `manta_relay_spots_total` counts spots an uplink
                // connection received; with none connected (reconnect
                // backoff) the spot is lost, as manta's own are (D6).
                if self.bus.publish_relay(spot) {
                    self.stats.record_spot();
                } else {
                    self.dedupe.forget(&key.0, key.1, key.2);
                }
            }
            Err(reason) => self.stats.record_dropped(reason),
        }
    }

    fn gate(&mut self, d: &Decode) -> Result<RelaySpot, DropReason> {
        if !d.new {
            return Err(DropReason::Replay);
        }
        if d.off_air {
            return Err(DropReason::OffAir);
        }
        if d.low_confidence {
            return Err(DropReason::LowConfidence);
        }
        let inst = self
            .instances
            .get(&d.id)
            .filter(|i| i.dial_freq_hz != 0)
            .ok_or(DropReason::NoDial)?;
        let mode = match inst.mode.as_str() {
            "FT8" => RelayMode::Ft8,
            "FT4" => RelayMode::Ft4,
            _ => return Err(DropReason::Mode),
        };
        let callsign = match cq_caller(&d.message) {
            CqCaller::NotCq => return Err(DropReason::NotCq),
            CqCaller::NoCall => return Err(DropReason::NoCall),
            CqCaller::Call(call) => call,
        };
        let freq_hz = inst.dial_freq_hz as f64 + f64::from(d.delta_freq_hz);
        let now = (self.clock)();
        if !self
            .dedupe
            .admit(&callsign, mode, band_for_freq_hz(freq_hz), now)
        {
            return Err(DropReason::Duplicate);
        }
        Ok(RelaySpot {
            spotter: self.spotter.clone(),
            callsign,
            freq_hz,
            mode,
            snr_db: d.snr,
            spot_type: manta_spot::SpotType::Cq,
            unix_ts: now,
        })
    }
}

/// Runs one WSJT-X relay over a pre-bound (nonblocking) socket until
/// shutdown.
pub async fn run(
    cfg: WsjtxRelay,
    socket: std::net::UdpSocket,
    clock: Clock,
    bus: Arc<SpotBus>,
    stats: Arc<RelayStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let socket = match tokio::net::UdpSocket::from_std(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("wsjtx_relay {}: cannot use the socket: {e}", cfg.label);
            return;
        }
    };
    tracing::info!(
        "wsjtx_relay {}: listening for WSJT-X; new FT8/FT4 CQ decodes go out as {} through [[rbn_uplink]]",
        cfg.label,
        cfg.callsign
    );
    let mut relay = Relay {
        spotter: Arc::from(cfg.callsign.as_str()),
        cfg,
        clock,
        bus,
        stats,
        instances: HashMap::new(),
        seq: 0,
        dedupe: Dedupe::default(),
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_heard: Option<Instant> = None;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let down_at = last_heard.map(|t| t + UP_WINDOW);
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = async { tokio::time::sleep_until(down_at.unwrap_or_else(Instant::now)).await },
                if down_at.is_some() =>
            {
                relay.stats.set_up(false);
                last_heard = None;
            }
            received = socket.recv_from(&mut buf) => match received {
                Ok((n, _from)) => {
                    // Any decodable datagram, heartbeats included, is "heard".
                    if relay.handle(&buf[..n]) {
                        relay.stats.set_up(true);
                        last_heard = Some(Instant::now());
                    }
                }
                Err(e) => {
                    tracing::debug!("wsjtx_relay {}: recv failed: {e}", relay.cfg.label);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(c: &str) -> CqCaller {
        CqCaller::Call(c.to_string())
    }

    #[test]
    fn cq_caller_reads_the_plans_table() {
        let cases: &[(&str, CqCaller)] = &[
            ("CQ K1ABC FN42", call("K1ABC")),
            ("CQ DX K1ABC FN42", call("K1ABC")),
            ("CQ POTA K1ABC FN42", call("K1ABC")),
            ("CQ TEST K1ABC FN42", call("K1ABC")),
            ("CQ NA K1ABC FN42", call("K1ABC")),
            ("CQ 290 K1ABC FN42", call("K1ABC")),
            ("CQ K1ABC", call("K1ABC")),
            ("CQ <K1ABC/P> FN42", call("K1ABC/P")),
            ("CQ <...> FN42", CqCaller::NoCall),
            ("CQ PJ4/K1ABC", call("PJ4/K1ABC")),
            ("K1ABC W9XYZ EN37", CqCaller::NotCq),
            ("K1ABC W9XYZ -12", CqCaller::NotCq),
            ("W9XYZ K1ABC RR73", CqCaller::NotCq),
            ("TNX 73 GL", CqCaller::NotCq),
            ("CQ", CqCaller::NoCall),
        ];
        for (msg, want) in cases {
            assert_eq!(&cq_caller(msg), want, "{msg}");
        }
    }

    #[test]
    fn a_modifier_that_is_itself_a_call_is_the_caller() {
        // "K1A" is short, but has a digit, so it is the caller.
        assert_eq!(cq_caller("CQ K1A FN42"), call("K1A"));
        assert_eq!(cq_caller("CQ DX"), CqCaller::NoCall);
    }

    #[test]
    fn dedupe_drops_within_600_s_and_admits_after() {
        let mut d = Dedupe::default();
        assert!(d.admit("K1ABC", RelayMode::Ft8, "20m", 1_000));
        assert!(!d.admit("K1ABC", RelayMode::Ft8, "20m", 1_599));
        assert!(d.admit("K1ABC", RelayMode::Ft8, "20m", 1_600));
        assert!(d.admit("K1ABC", RelayMode::Ft4, "20m", 1_600));
        assert!(d.admit("K1ABC", RelayMode::Ft8, "40m", 1_600));
        assert!(!d.admit("K1ABC", RelayMode::Ft8, "40m", 1_601));
    }

    #[test]
    fn a_full_dedupe_prunes_expired_entries_then_evicts_the_oldest() {
        let mut d = Dedupe::with_capacity(3);
        assert!(d.admit("K1AA", RelayMode::Ft8, "20m", 0));
        assert!(d.admit("K1AB", RelayMode::Ft8, "20m", 10));
        assert!(d.admit("K1AC", RelayMode::Ft8, "20m", 700));
        // Full at 705: K1AA (0) and K1AB (10) are expired and pruned.
        assert!(d.admit("K1AD", RelayMode::Ft8, "20m", 705));
        assert_eq!(d.len(), 2);
        assert!(d.admit("K1AE", RelayMode::Ft8, "20m", 706));
        // Full, nothing expired: the oldest (K1AC at 700) is evicted.
        assert!(d.admit("K1AF", RelayMode::Ft8, "20m", 707));
        assert_eq!(d.len(), 3);
        assert!(
            d.admit("K1AC", RelayMode::Ft8, "20m", 708),
            "K1AC was evicted"
        );
        assert!(!d.admit("K1AF", RelayMode::Ft8, "20m", 709));
    }
}
