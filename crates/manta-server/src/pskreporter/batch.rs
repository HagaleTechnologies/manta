//! MAN-142 D5: which spots PSKReporter gets and in which datagram. A
//! per-(call, band) gate admits a call at most once every
//! [`REPORT_GATE_SECS`], timed by the spot's own Unix time; admitted
//! reports wait for the next flush, or go out early when the next one
//! would take the datagram past [`MAX_DATAGRAM_BYTES`]. Templates ride in
//! the first three datagrams and then hourly. Every time is passed in, so
//! the rules are tested without a clock.

use super::ipfix::{self, Header, Receiver, Report};
use super::PskDrop;
use crate::config::UplinkSpotTypes;
use manta_spot::Spot;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// The page's hard rule: a call at most once per five minutes (per band).
pub const REPORT_GATE_SECS: i64 = 300;
/// At most one datagram per five minutes, unless one fills.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(300);
/// Templates again in the first datagram this long after the last.
pub const TEMPLATE_INTERVAL: Duration = Duration::from_secs(3600);
/// A datagram never grows past this.
pub const MAX_DATAGRAM_BYTES: usize = 1400;
/// The gate map's bound.
pub const MAX_GATE_ENTRIES: usize = 20_000;
/// Datagrams at session start that carry the templates.
pub const TEMPLATED_STARTUP_DATAGRAMS: u64 = 3;

/// One datagram to send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub bytes: Vec<u8>,
    /// Reports it carries.
    pub reports: usize,
    /// Whether it carries the templates (the worker re-resolves the host
    /// before such a send, D9).
    pub templates: bool,
}

/// What [`Batcher::offer`] did with a spot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Pending for the next datagram.
    Admitted,
    /// Not reported.
    Dropped(PskDrop),
    /// Admitted, and the datagram it could not fit into is due now; the
    /// spot starts the next one.
    Full(Datagram),
}

/// The report state of one session.
#[derive(Debug)]
pub struct Batcher {
    receiver: Receiver,
    spot_types: UplinkSpotTypes,
    observation_domain: u32,
    /// Last admitted Unix time per (upper-cased call, band).
    gate: HashMap<(String, &'static str), i64>,
    pending: Vec<Report>,
    pending_len: usize,
    /// Reports in datagrams already built (the header's sequence number).
    sequence: u32,
    datagrams: u64,
    last_templates: Option<Instant>,
    /// The newest spot time offered, for pruning the gate.
    latest_ts: i64,
}

/// The report for `spot`, heard at `unix_ts` (D8, D11).
pub fn report_for(spot: &Spot, unix_ts: i64) -> Report {
    let freq = spot.freq_hz;
    let freq_hz = if freq.is_finite() && freq > 0.0 {
        freq.round() as u64
    } else {
        0
    };
    Report {
        callsign: spot.callsign.to_ascii_uppercase(),
        freq_hz,
        snr_db: ipfix::snr_i8(spot.snr_db),
        mode: spot.mode.label(),
        flow_start: unix_ts.clamp(0, i64::from(u32::MAX)) as u32,
    }
}

impl Batcher {
    pub fn new(receiver: Receiver, spot_types: UplinkSpotTypes, observation_domain: u32) -> Self {
        Self {
            receiver,
            spot_types,
            observation_domain,
            gate: HashMap::new(),
            pending: Vec::new(),
            pending_len: 0,
            sequence: 0,
            datagrams: 0,
            last_templates: None,
            latest_ts: i64::MIN,
        }
    }

    /// Reports waiting for the next datagram.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Entries in the gate map.
    pub fn gate_len(&self) -> usize {
        self.gate.len()
    }

    /// Whether a datagram built at `now` carries the templates.
    pub fn templates_due(&self, now: Instant) -> bool {
        self.datagrams < TEMPLATED_STARTUP_DATAGRAMS
            || self
                .last_templates
                .is_none_or(|t| now.saturating_duration_since(t) >= TEMPLATE_INTERVAL)
    }

    /// The last datagram built carried templates but never reached the
    /// OS (resolve or send failure): the next one carries them again, so
    /// PSKReporter never gets data sets it has no templates for.
    pub fn templates_not_sent(&mut self) {
        self.datagrams = self.datagrams.saturating_sub(1);
        self.last_templates = None;
    }

    /// Offers one validated spot heard at `unix_ts`. `now` and
    /// `export_time` date the datagram returned in [`Offer::Full`].
    pub fn offer(&mut self, spot: &Spot, unix_ts: i64, now: Instant, export_time: u32) -> Offer {
        if !self.spot_types.forwards(spot.spot_type) {
            return Offer::Dropped(PskDrop::SpotType);
        }
        self.latest_ts = self.latest_ts.max(unix_ts);
        let key = (
            spot.callsign.to_ascii_uppercase(),
            crate::band::band_for_freq_hz(spot.freq_hz),
        );
        if let Some(&last) = self.gate.get(&key) {
            if unix_ts - last < REPORT_GATE_SECS {
                return Offer::Dropped(PskDrop::RateGate);
            }
        } else if self.gate.len() >= MAX_GATE_ENTRIES {
            self.prune(unix_ts);
            if self.gate.len() >= MAX_GATE_ENTRIES {
                if let Some(oldest) = self
                    .gate
                    .iter()
                    .min_by_key(|(_, t)| **t)
                    .map(|(k, _)| k.clone())
                {
                    self.gate.remove(&oldest);
                }
            }
        }
        self.gate.insert(key, unix_ts);
        let report = report_for(spot, unix_ts);
        let len = ipfix::report_len(&report);
        let full = if !self.pending.is_empty()
            && ipfix::datagram_len(
                &self.receiver,
                self.templates_due(now),
                self.pending_len + len,
            ) > MAX_DATAGRAM_BYTES
        {
            self.build(now, export_time)
        } else {
            None
        };
        self.pending.push(report);
        self.pending_len += len;
        match full {
            Some(d) => Offer::Full(d),
            None => Offer::Admitted,
        }
    }

    /// The datagram due at a flush, or `None` with nothing pending. Also
    /// prunes the gate of entries older than [`REPORT_GATE_SECS`].
    pub fn flush(&mut self, now: Instant, export_time: u32) -> Option<Datagram> {
        if self.latest_ts != i64::MIN {
            self.prune(self.latest_ts);
        }
        self.build(now, export_time)
    }

    fn prune(&mut self, now_ts: i64) {
        self.gate
            .retain(|_, t| now_ts.saturating_sub(*t) < REPORT_GATE_SECS);
    }

    fn build(&mut self, now: Instant, export_time: u32) -> Option<Datagram> {
        if self.pending.is_empty() {
            return None;
        }
        let templates = self.templates_due(now);
        let reports = std::mem::take(&mut self.pending);
        self.pending_len = 0;
        let bytes = ipfix::datagram(
            &Header {
                export_time,
                sequence: self.sequence,
                observation_domain: self.observation_domain,
            },
            &self.receiver,
            templates,
            &reports,
        );
        self.sequence = self.sequence.wrapping_add(reports.len() as u32);
        self.datagrams += 1;
        if templates {
            self.last_templates = Some(now);
        }
        Some(Datagram {
            bytes,
            reports: reports.len(),
            templates,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

    fn receiver() -> Receiver {
        Receiver {
            callsign: "W5AU".into(),
            locator: "EM10dh".into(),
            software: "manta 0.1.0".into(),
            antenna: String::new(),
        }
    }

    fn spot(call: &str, freq_hz: f64, spot_type: SpotType) -> Spot {
        Spot {
            callsign: call.to_string(),
            freq_hz,
            snr_db: 21.0,
            wpm: 20.0,
            spot_type,
            confidence: 0.9,
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
            mode: manta_spot::SpotMode::Cw,
            baud: None,
        }
    }

    fn batcher() -> Batcher {
        Batcher::new(receiver(), UplinkSpotTypes::CqBeacon, 7)
    }

    const T: i64 = 1_793_650_021;

    fn templated(d: &Datagram) -> bool {
        // The first set after the header is the options template set (3).
        u16::from_be_bytes([d.bytes[16], d.bytes[17]]) == 3
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_is_reported_once_per_band_per_300_s() {
        let mut b = batcher();
        let now = Instant::now();
        let s = spot("W1AW", 14_012_340.0, SpotType::Cq);
        assert_eq!(b.offer(&s, T, now, 0), Offer::Admitted);
        let louder = Spot {
            snr_db: 27.0,
            ..s.clone()
        };
        assert_eq!(
            b.offer(&louder, T + 120, now, 0),
            Offer::Dropped(PskDrop::RateGate)
        );
        assert_eq!(
            b.offer(&s, T + 299, now, 0),
            Offer::Dropped(PskDrop::RateGate)
        );
        assert_eq!(b.pending(), 1);
        assert_eq!(b.offer(&s, T + 300, now, 0), Offer::Admitted);
        assert_eq!(b.pending(), 2);
        // The gate ignores the call's case.
        let lower = spot("w1aw", 14_012_340.0, SpotType::Cq);
        assert_eq!(
            b.offer(&lower, T + 301, now, 0),
            Offer::Dropped(PskDrop::RateGate)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_call_on_another_band_is_a_new_report() {
        let mut b = batcher();
        let now = Instant::now();
        assert_eq!(
            b.offer(&spot("W1AW", 14_012_340.0, SpotType::Cq), T, now, 0),
            Offer::Admitted
        );
        assert_eq!(
            b.offer(&spot("W1AW", 7_024_000.0, SpotType::Cq), T + 10, now, 0),
            Offer::Admitted
        );
        assert_eq!(b.pending(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_pending_flushes_to_nothing() {
        let mut b = batcher();
        assert_eq!(b.flush(Instant::now(), 0), None);
        b.offer(
            &spot("W1AW", 14_012_340.0, SpotType::Cq),
            T,
            Instant::now(),
            0,
        );
        assert!(b.flush(Instant::now(), 0).is_some());
        assert_eq!(b.flush(Instant::now(), 0), None);
    }

    #[tokio::test(start_paused = true)]
    async fn templates_ride_in_the_first_three_datagrams_then_hourly() {
        let mut b = batcher();
        let start = Instant::now();
        let mut sent = Vec::new();
        // One datagram every 300 s, each with a new call.
        for i in 0..16u64 {
            let now = start + Duration::from_secs(300 * i);
            let call = format!("K{i}ABC");
            b.offer(
                &spot(&call, 14_012_340.0, SpotType::Cq),
                T + 300 * i as i64,
                now,
                0,
            );
            let d = b.flush(now, 0).expect("a datagram");
            assert_eq!(d.templates, templated(&d));
            sent.push((300 * i, d.templates));
        }
        // 0, 300, 600 carry templates; 900 does not; the last templated one
        // was at 600, so 4200 (600 + 3600) is the next.
        let with: Vec<u64> = sent.iter().filter(|(_, t)| *t).map(|(at, _)| *at).collect();
        assert_eq!(with, vec![0, 300, 600, 4200]);
        assert!(!sent[3].1, "datagram 4 at +900 s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_datagram_is_flushed_early_before_it_passes_1400_bytes() {
        let mut b = batcher();
        let now = Instant::now();
        let mut full = None;
        let mut offered = 0;
        for i in 0..200 {
            let call = format!("K{:02}ABC", i % 100);
            let call = if i >= 100 {
                format!("W{}", &call[1..])
            } else {
                call
            };
            assert_eq!(call.len(), 6);
            offered += 1;
            match b.offer(&spot(&call, 14_012_340.0, SpotType::Cq), T, now, 0) {
                Offer::Admitted => {}
                Offer::Full(d) => {
                    full = Some(d);
                    break;
                }
                Offer::Dropped(r) => panic!("dropped {r:?}"),
            }
        }
        let d = full.expect("a datagram fills");
        assert!(d.bytes.len() <= MAX_DATAGRAM_BYTES, "{}", d.bytes.len());
        assert_eq!(d.reports, offered - 1);
        // One more 22-byte report would not have fit.
        assert!(d.bytes.len() + 22 > MAX_DATAGRAM_BYTES);
        // The overflowing report starts the next batch.
        assert_eq!(b.pending(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_sequence_number_counts_earlier_reports() {
        let mut b = batcher();
        let now = Instant::now();
        let mut seqs = Vec::new();
        let mut n = 0;
        for batch in [2, 3, 1] {
            for _ in 0..batch {
                b.offer(
                    &spot(&format!("K{n}AA"), 14_012_340.0, SpotType::Cq),
                    T,
                    now,
                    0,
                );
                n += 1;
            }
            let d = b.flush(now, 0).unwrap();
            seqs.push(u32::from_be_bytes([
                d.bytes[8],
                d.bytes[9],
                d.bytes[10],
                d.bytes[11],
            ]));
        }
        assert_eq!(seqs, vec![0, 2, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn spot_types_cq_beacon_holds_back_de_and_unknown() {
        let now = Instant::now();
        let mut b = batcher();
        for (i, (t, want)) in [
            (SpotType::Cq, Offer::Admitted),
            (SpotType::Beacon, Offer::Admitted),
            (SpotType::De, Offer::Dropped(PskDrop::SpotType)),
            (SpotType::Unknown, Offer::Dropped(PskDrop::SpotType)),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                b.offer(&spot(&format!("K{i}AA"), 14_012_340.0, t), T, now, 0),
                want
            );
        }
        let mut all = Batcher::new(receiver(), UplinkSpotTypes::All, 7);
        for (i, t) in [
            SpotType::Cq,
            SpotType::Beacon,
            SpotType::De,
            SpotType::Unknown,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                all.offer(&spot(&format!("K{i}AA"), 14_012_340.0, t), T, now, 0),
                Offer::Admitted
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn templates_that_were_not_sent_are_due_again() {
        let mut b = Batcher::new(receiver(), UplinkSpotTypes::All, 1);
        let t0 = Instant::now();
        b.datagrams = TEMPLATED_STARTUP_DATAGRAMS;
        b.last_templates = Some(t0);
        assert!(!b.templates_due(t0));
        b.templates_not_sent();
        assert!(b.templates_due(t0));
    }

    #[tokio::test(start_paused = true)]
    async fn the_gate_map_is_bounded() {
        let mut b = batcher();
        let now = Instant::now();
        for i in 0..=MAX_GATE_ENTRIES {
            let call = format!("K{i}");
            b.offer(&spot(&call, 14_012_340.0, SpotType::Cq), T, now, 0);
            // Keep `pending` small; the gate is what is under test.
            if b.pending() > 50 {
                b.flush(now, 0);
            }
            assert!(b.gate_len() <= MAX_GATE_ENTRIES);
        }
        assert_eq!(b.gate_len(), MAX_GATE_ENTRIES);
        // Old entries are pruned at a flush.
        b.offer(&spot("W1AW", 14_012_340.0, SpotType::Cq), T + 600, now, 0);
        b.flush(now, 0);
        assert_eq!(b.gate_len(), 1);
    }
}
