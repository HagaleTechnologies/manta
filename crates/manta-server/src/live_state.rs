//! MAN-324: the live-state stream's wire records and band rollup. The
//! decode loop's records (`manta_engine::LiveRecord`) are mapped onto these
//! in `manta-cli`, serialised once and fanned out on the live channel by
//! `json_stream::serve` (`StreamKind::Live`). Every field is documented in
//! `docs/LIVE-STATE.md`; `every_live_record_field_is_documented` keeps the
//! two in step.

use crate::band::{allocations, band_for_freq_hz};
use manta_spot::{Decision, Reason, Support, Verdict};
use serde::Serialize;

/// Every `kind` the live stream carries, in `/metrics` label order.
pub const LIVE_RECORD_KINDS: [&str; 4] = [
    BAND_STATE_KIND,
    TRACK_STATE_KIND,
    CANDIDATE_DECISION_KIND,
    TRACK_CLOSED_KIND,
];
pub const BAND_STATE_KIND: &str = "bandState";
pub const TRACK_STATE_KIND: &str = "trackState";
pub const CANDIDATE_DECISION_KIND: &str = "candidateDecision";
pub const TRACK_CLOSED_KIND: &str = "trackClosed";

/// The pipeline's native SNR reference bandwidth, quoted on `trackState`.
pub const SNR_REF_HZ: u32 = 2500;

/// One channel, as the band rollup reads it. Calibrated Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelLevel {
    pub freq_hz: f64,
    pub floor_db: f32,
    pub occupied: bool,
}

/// What the band rollup summarises: every channel, plus the calibrated
/// frequency of every track holding a decoder.
#[derive(Debug, Clone, Copy)]
pub struct BandInput<'a> {
    pub channels: &'a [ChannelLevel],
    pub decoding_track_freqs_hz: &'a [f64],
}

/// One `bands[]` entry of a `bandState` record.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BandRow {
    pub band: &'static str,
    pub lo_hz: i64,
    pub hi_hz: i64,
    pub channels: usize,
    pub noise_floor_db: f64,
    pub occupancy: f64,
    pub active_tracks: usize,
}

fn band_order(band: &str) -> usize {
    allocations()
        .iter()
        .position(|(name, _, _)| *name == band)
        .unwrap_or(usize::MAX)
}

fn round_to(x: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (x * scale).round() / scale
}

fn median(values: &mut [f32]) -> f64 {
    values.sort_by(f32::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        f64::from(values[n / 2])
    } else {
        (f64::from(values[n / 2 - 1]) + f64::from(values[n / 2])) / 2.0
    }
}

/// Groups channels by band (`band_for_freq_hz` of each channel's centre)
/// in band-table order with `unknown` last: each band's channel span,
/// median effective floor (0.1 dB), occupied share (4 decimals) and the
/// decoding tracks whose frequency falls in it. Only bands with at least
/// one channel appear.
pub fn band_rollup(input: BandInput<'_>) -> Vec<BandRow> {
    let mut groups: std::collections::BTreeMap<(usize, &'static str), Vec<ChannelLevel>> =
        std::collections::BTreeMap::new();
    for c in input.channels {
        let band = band_for_freq_hz(c.freq_hz.round());
        groups.entry((band_order(band), band)).or_default().push(*c);
    }
    groups
        .into_iter()
        .map(|((_, band), chans)| {
            let lo = chans
                .iter()
                .map(|c| c.freq_hz)
                .fold(f64::INFINITY, f64::min);
            let hi = chans
                .iter()
                .map(|c| c.freq_hz)
                .fold(f64::NEG_INFINITY, f64::max);
            let mut floors: Vec<f32> = chans.iter().map(|c| c.floor_db).collect();
            let occupied = chans.iter().filter(|c| c.occupied).count();
            let active_tracks = input
                .decoding_track_freqs_hz
                .iter()
                .filter(|f| band_for_freq_hz(f.round()) == band)
                .count();
            BandRow {
                band,
                lo_hz: lo.round() as i64,
                hi_hz: hi.round() as i64,
                channels: chans.len(),
                noise_floor_db: round_to(median(&mut floors), 1),
                occupancy: round_to(occupied as f64 / chans.len() as f64, 4),
                active_tracks,
            }
        })
        .collect()
}

/// `bandState`: once per second of stream time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BandStateRecord {
    pub kind: &'static str,
    pub de_call: String,
    /// MAN-13: the `[[input]]` name this record describes; absent with one
    /// input, whose record covers the whole passband.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub timestamp: i64,
    pub bands: Vec<BandRow>,
}

impl BandStateRecord {
    pub fn new(de_call: &str, timestamp: i64, bands: Vec<BandRow>) -> Self {
        Self {
            kind: BAND_STATE_KIND,
            de_call: de_call.to_string(),
            source: None,
            timestamp,
            bands,
        }
    }

    /// MAN-13: names the input the record describes.
    pub fn with_source(mut self, source: &str) -> Self {
        self.source = Some(source.to_string());
        self
    }
}

/// A candidate callsign with its support: a `candidates[]` entry, or a
/// `candidateDecision`'s `rival`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateRow {
    pub call: String,
    pub reps: u32,
    pub conf_sum: f64,
}

impl CandidateRow {
    pub fn new(call: &str, support: &Support) -> Self {
        Self {
            call: call.to_string(),
            reps: support.reps,
            conf_sum: round_to(f64::from(support.conf_sum), 3),
        }
    }
}

/// `trackState`: a track's running copy, when its text changed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackStateRecord {
    pub kind: &'static str,
    pub de_call: String,
    pub timestamp: i64,
    pub track_id: u32,
    pub frequency: Option<i64>,
    pub band: Option<&'static str>,
    pub wpm: Option<i64>,
    pub snr: Option<i64>,
    pub snr_ref_hz: u32,
    pub text: String,
    pub candidates: Vec<CandidateRow>,
}

/// The engine-independent fields of a `trackState`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackStateInput<'a> {
    pub track_id: u32,
    pub freq_hz: Option<f64>,
    pub wpm: Option<f32>,
    pub snr_2500_db: Option<f32>,
    pub text: &'a str,
    pub candidates: Vec<CandidateRow>,
}

impl TrackStateRecord {
    pub fn new(de_call: &str, timestamp: i64, t: TrackStateInput<'_>) -> Self {
        let frequency = t.freq_hz.map(|f| f.round() as i64);
        Self {
            kind: TRACK_STATE_KIND,
            de_call: de_call.to_string(),
            timestamp,
            track_id: t.track_id,
            frequency,
            band: frequency.map(|f| band_for_freq_hz(f as f64)),
            wpm: t.wpm.map(|w| w.round() as i64),
            snr: t.snr_2500_db.map(|s| s.round() as i64),
            snr_ref_hz: SNR_REF_HZ,
            text: t.text.to_string(),
            candidates: t.candidates,
        }
    }
}

/// `candidateDecision`: one validation decision and its reason.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateDecisionRecord {
    pub kind: &'static str,
    pub de_call: String,
    pub timestamp: i64,
    pub track_id: u32,
    pub call: String,
    pub frequency: i64,
    pub band: &'static str,
    pub spot_type: &'static str,
    pub decision: &'static str,
    pub reason: Option<&'static str>,
    pub reps: Option<u32>,
    pub required_reps: u32,
    pub pattern_tier: Option<&'static str>,
    pub rival: Option<CandidateRow>,
    pub spot_id: Option<String>,
}

impl CandidateDecisionRecord {
    /// `spot_id` is the `:7301` spot `id` of a spotted decision's spot.
    pub fn new(de_call: &str, timestamp: i64, d: &Decision, spot_id: Option<String>) -> Self {
        let frequency = d.freq_hz.round() as i64;
        Self {
            kind: CANDIDATE_DECISION_KIND,
            de_call: de_call.to_string(),
            timestamp,
            track_id: d.track_id,
            call: d.call.clone(),
            frequency,
            band: band_for_freq_hz(frequency as f64),
            spot_type: crate::metrics::spot_type_metric_label(d.spot_type),
            decision: decision_label(d.verdict),
            reason: d.reason.map(reason_label),
            reps: d.reps,
            required_reps: d.required_reps,
            pattern_tier: d.pattern_tier.map(|tier| tier.as_str()),
            rival: d
                .rival
                .as_ref()
                .map(|r| CandidateRow::new(&r.call, &r.support)),
            spot_id,
        }
    }
}

/// How a track closed (`manta_decode::events::ClosureKind`, which this
/// crate does not depend on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackClosure {
    SignalEnded,
    /// Merged into the survivor, or evicted (`None`).
    Bookkeeping(Option<u32>),
}

/// `trackClosed`: a track ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackClosedRecord {
    pub kind: &'static str,
    pub de_call: String,
    pub timestamp: i64,
    pub track_id: u32,
    pub closure: &'static str,
    pub survivor_track_id: Option<u32>,
}

impl TrackClosedRecord {
    pub fn new(de_call: &str, timestamp: i64, track_id: u32, closure: TrackClosure) -> Self {
        let (closure, survivor_track_id) = match closure {
            TrackClosure::SignalEnded => ("signalEnded", None),
            TrackClosure::Bookkeeping(survivor) => ("bookkeeping", survivor),
        };
        Self {
            kind: TRACK_CLOSED_KIND,
            de_call: de_call.to_string(),
            timestamp,
            track_id,
            closure,
            survivor_track_id,
        }
    }
}

/// `decision` labels, as documented in `docs/LIVE-STATE.md`.
pub fn decision_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Spotted => "spotted",
        Verdict::Withheld => "withheld",
        Verdict::Deferred => "deferred",
    }
}

/// `reason` labels, as documented in `docs/LIVE-STATE.md`.
pub fn reason_label(r: Reason) -> &'static str {
    match r {
        Reason::Blocklist => "blocklist",
        Reason::Notch => "notch",
        Reason::Grammar => "grammar",
        Reason::Cty => "cty",
        Reason::Repetition => "repetition",
        Reason::Variant => "variant",
        Reason::Dedupe => "dedupe",
        Reason::PowerStepGuard => "powerStepGuard",
        Reason::BeaconPending => "beaconPending",
        Reason::BeaconSpeed => "beaconSpeed",
        Reason::BeaconOverflow => "beaconOverflow",
        Reason::BeaconDiscarded => "beaconDiscarded",
        Reason::LookupPending => "lookupPending",
        Reason::LookupNotFound => "lookupNotFound",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::{PatternTier, Rival, Spot, SpotType};

    fn ch(freq_hz: f64, floor_db: f32, occupied: bool) -> ChannelLevel {
        ChannelLevel {
            freq_hz,
            floor_db,
            occupied,
        }
    }

    fn rollup(channels: &[ChannelLevel], tracks: &[f64]) -> Vec<BandRow> {
        band_rollup(BandInput {
            channels,
            decoding_track_freqs_hz: tracks,
        })
    }

    #[test]
    fn band_rollup_groups_channels_by_band_in_table_order_with_unknown_last() {
        let channels = [
            ch(13_999_000.0, -60.0, false),
            ch(7_100_000.0, -60.0, false),
            ch(14_001_000.0, -60.0, false),
            ch(3_600_000.0, -60.0, false),
        ];
        let bands: Vec<&str> = rollup(&channels, &[]).iter().map(|b| b.band).collect();
        assert_eq!(bands, ["80m", "40m", "20m", "unknown"]);
    }

    #[test]
    fn band_rollup_floor_is_the_median_and_occupancy_the_occupied_share() {
        let channels = [
            ch(14_001_000.0, -60.0, true),
            ch(14_002_000.0, -62.0, false),
            ch(14_003_000.0, -70.0, false),
            ch(14_004_000.0, -61.0, false),
        ];
        let rows = rollup(&channels, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].noise_floor_db, -61.5);
        assert_eq!(rows[0].occupancy, 0.25);
        assert_eq!((rows[0].lo_hz, rows[0].hi_hz), (14_001_000, 14_004_000));
        assert_eq!(rows[0].channels, 4);
    }

    #[test]
    fn band_rollup_counts_decoding_tracks_per_band() {
        let channels = [
            ch(13_999_000.0, -60.0, false),
            ch(14_001_000.0, -60.0, false),
        ];
        let rows = rollup(&channels, &[14_012_000.0, 14_020_000.0, 13_990_000.0]);
        assert_eq!(rows[0].band, "20m");
        assert_eq!(rows[0].active_tracks, 2);
        assert_eq!(rows[1].band, "unknown");
        assert_eq!(rows[1].active_tracks, 1);
    }

    #[test]
    fn band_rollup_of_an_audio_passband_is_one_unknown_segment() {
        let channels: Vec<ChannelLevel> = (0..256)
            .map(|k| ch(f64::from(k) * 93.75, -80.0, false))
            .collect();
        let rows = rollup(&channels, &[1_000.0]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].band, "unknown");
        assert_eq!(rows[0].channels, 256);
        assert_eq!(rows[0].active_tracks, 1);
    }

    #[test]
    fn band_rollup_of_no_channels_is_empty() {
        assert!(rollup(&[], &[14_000_100.0]).is_empty());
    }

    fn sample_band_state() -> BandStateRecord {
        BandStateRecord::new(
            "W1AW",
            1_700_000_005,
            vec![BandRow {
                band: "20m",
                lo_hz: 14_000_000,
                hi_hz: 14_047_906,
                channels: 512,
                noise_floor_db: -62.2,
                occupancy: 0.0059,
                active_tracks: 1,
            }],
        )
    }

    #[test]
    fn band_state_names_its_input_only_when_given_one() {
        let one = serde_json::to_value(sample_band_state()).unwrap();
        assert!(one.get("source").is_none(), "{one}");
        let named = serde_json::to_value(sample_band_state().with_source("20m")).unwrap();
        assert_eq!(named["source"], "20m");
    }

    fn sample_track_state() -> TrackStateRecord {
        TrackStateRecord::new(
            "W1AW",
            1_700_000_013,
            TrackStateInput {
                track_id: 1,
                freq_hz: Some(14_012_362.4),
                wpm: Some(20.2),
                snr_2500_db: Some(21.3),
                text: "CQ CQ DE W1AW",
                candidates: vec![CandidateRow::new(
                    "W1AW",
                    &Support {
                        reps: 1,
                        conf_sum: 0.55,
                    },
                )],
            },
        )
    }

    fn decision(verdict: Verdict, reason: Option<Reason>) -> Decision {
        Decision {
            track_id: 9,
            sample_ts: 0,
            call: "W4KTNL".into(),
            freq_hz: 14_031_200.0,
            spot_type: SpotType::De,
            verdict,
            reason,
            reps: Some(2),
            required_reps: 5,
            pattern_tier: Some(PatternTier::Unlisted),
            rival: None,
            spot: None,
        }
    }

    #[test]
    fn band_state_line() {
        assert_eq!(
            serde_json::to_string(&sample_band_state()).unwrap(),
            r#"{"kind":"bandState","deCall":"W1AW","timestamp":1700000005,"bands":[{"band":"20m","loHz":14000000,"hiHz":14047906,"channels":512,"noiseFloorDb":-62.2,"occupancy":0.0059,"activeTracks":1}]}"#
        );
    }

    #[test]
    fn track_state_line() {
        assert_eq!(
            serde_json::to_string(&sample_track_state()).unwrap(),
            r#"{"kind":"trackState","deCall":"W1AW","timestamp":1700000013,"trackId":1,"frequency":14012362,"band":"20m","wpm":20,"snr":21,"snrRefHz":2500,"text":"CQ CQ DE W1AW","candidates":[{"call":"W1AW","reps":1,"confSum":0.55}]}"#
        );
        let empty = TrackStateRecord::new(
            "W1AW",
            1,
            TrackStateInput {
                track_id: 2,
                freq_hz: None,
                wpm: None,
                snr_2500_db: None,
                text: "",
                candidates: vec![],
            },
        );
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"kind":"trackState","deCall":"W1AW","timestamp":1,"trackId":2,"frequency":null,"band":null,"wpm":null,"snr":null,"snrRefHz":2500,"text":"","candidates":[]}"#
        );
    }

    #[test]
    fn unlisted_pattern_decision_reports_five_required_repetitions() {
        let d = decision(Verdict::Withheld, Some(Reason::Repetition));
        let line =
            serde_json::to_string(&CandidateDecisionRecord::new("W1AW", 1, &d, None)).unwrap();
        assert!(
            line.contains(
                r#""reason":"repetition","reps":2,"requiredReps":5,"patternTier":"unlisted""#
            ),
            "{line}"
        );
    }

    #[test]
    fn allowlisted_decision_reports_one_required_repetition_and_no_pattern_tier() {
        let mut d = decision(Verdict::Spotted, None);
        d.reps = Some(1);
        d.required_reps = 1;
        d.pattern_tier = None;
        let line =
            serde_json::to_string(&CandidateDecisionRecord::new("W1AW", 1, &d, None)).unwrap();
        assert!(
            line.contains(r#""reps":1,"requiredReps":1,"patternTier":null"#),
            "{line}"
        );
    }

    #[test]
    fn candidate_decision_line_for_each_reason() {
        let mut d = decision(Verdict::Withheld, Some(Reason::Variant));
        d.rival = Some(Rival {
            call: "W4KCL".into(),
            support: Support {
                reps: 2,
                conf_sum: 1.016,
            },
        });
        assert_eq!(
            serde_json::to_string(&CandidateDecisionRecord::new(
                "W1AW",
                1_700_000_044,
                &d,
                None
            ))
            .unwrap(),
            r#"{"kind":"candidateDecision","deCall":"W1AW","timestamp":1700000044,"trackId":9,"call":"W4KTNL","frequency":14031200,"band":"20m","spotType":"de","decision":"withheld","reason":"variant","reps":2,"requiredReps":5,"patternTier":"unlisted","rival":{"call":"W4KCL","reps":2,"confSum":1.016},"spotId":null}"#
        );
        let spot = Spot {
            callsign: "W4KTNL".into(),
            freq_hz: 14_031_200.0,
            snr_db: 10.0,
            wpm: 20.0,
            spot_type: SpotType::De,
            confidence: 0.9,
            track_id: 9,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
        };
        let mut spotted = decision(Verdict::Spotted, None);
        spotted.spot = Some(spot);
        spotted.reps = Some(5);
        assert_eq!(
            serde_json::to_string(&CandidateDecisionRecord::new(
                "W1AW",
                1,
                &spotted,
                Some("W1AW:1:9:0:W4KTNL".into())
            ))
            .unwrap(),
            r#"{"kind":"candidateDecision","deCall":"W1AW","timestamp":1,"trackId":9,"call":"W4KTNL","frequency":14031200,"band":"20m","spotType":"de","decision":"spotted","reason":null,"reps":5,"requiredReps":5,"patternTier":"unlisted","rival":null,"spotId":"W1AW:1:9:0:W4KTNL"}"#
        );
        let cases = [
            (
                Verdict::Withheld,
                Reason::Blocklist,
                "withheld",
                "blocklist",
            ),
            (Verdict::Withheld, Reason::Notch, "withheld", "notch"),
            (Verdict::Withheld, Reason::Grammar, "withheld", "grammar"),
            (Verdict::Withheld, Reason::Cty, "withheld", "cty"),
            (
                Verdict::Withheld,
                Reason::Repetition,
                "withheld",
                "repetition",
            ),
            (Verdict::Withheld, Reason::Variant, "withheld", "variant"),
            (Verdict::Withheld, Reason::Dedupe, "withheld", "dedupe"),
            (
                Verdict::Withheld,
                Reason::PowerStepGuard,
                "withheld",
                "powerStepGuard",
            ),
            (
                Verdict::Deferred,
                Reason::BeaconPending,
                "deferred",
                "beaconPending",
            ),
            (
                Verdict::Withheld,
                Reason::BeaconSpeed,
                "withheld",
                "beaconSpeed",
            ),
            (
                Verdict::Withheld,
                Reason::BeaconOverflow,
                "withheld",
                "beaconOverflow",
            ),
            (
                Verdict::Withheld,
                Reason::BeaconDiscarded,
                "withheld",
                "beaconDiscarded",
            ),
            (
                Verdict::Deferred,
                Reason::LookupPending,
                "deferred",
                "lookupPending",
            ),
            (
                Verdict::Withheld,
                Reason::LookupNotFound,
                "withheld",
                "lookupNotFound",
            ),
        ];
        for (verdict, reason, decision_s, reason_s) in cases {
            let line = serde_json::to_string(&CandidateDecisionRecord::new(
                "W1AW",
                1,
                &decision(verdict, Some(reason)),
                None,
            ))
            .unwrap();
            assert!(
                line.contains(&format!(
                    r#""decision":"{decision_s}","reason":"{reason_s}","#
                )),
                "{line}"
            );
        }
    }

    #[test]
    fn track_closed_line() {
        assert_eq!(
            serde_json::to_string(&TrackClosedRecord::new(
                "W1AW",
                1_700_000_120,
                1,
                TrackClosure::SignalEnded
            ))
            .unwrap(),
            r#"{"kind":"trackClosed","deCall":"W1AW","timestamp":1700000120,"trackId":1,"closure":"signalEnded","survivorTrackId":null}"#
        );
        assert_eq!(
            serde_json::to_string(&TrackClosedRecord::new(
                "W1AW",
                5,
                3,
                TrackClosure::Bookkeeping(Some(2))
            ))
            .unwrap(),
            r#"{"kind":"trackClosed","deCall":"W1AW","timestamp":5,"trackId":3,"closure":"bookkeeping","survivorTrackId":2}"#
        );
    }

    /// Every serialized key (nested `bands[]`, `candidates[]` and `rival`
    /// keys included) has a `| `field` |` row in its kind's section of
    /// `docs/LIVE-STATE.md`, and every row names a key.
    #[test]
    fn every_live_record_field_is_documented() {
        fn keys(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, child) in map {
                        out.insert(k.clone());
                        keys(child, out);
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|i| keys(i, out)),
                _ => {}
            }
        }
        let mut variant = decision(Verdict::Withheld, Some(Reason::Variant));
        variant.rival = Some(Rival {
            call: "W4KCL".into(),
            support: Support::default(),
        });
        let samples = [
            // MAN-13: `source` is serialized with several inputs.
            serde_json::to_value(sample_band_state().with_source("20m")).unwrap(),
            serde_json::to_value(sample_track_state()).unwrap(),
            serde_json::to_value(CandidateDecisionRecord::new("W1AW", 1, &variant, None)).unwrap(),
            serde_json::to_value(TrackClosedRecord::new(
                "W1AW",
                1,
                1,
                TrackClosure::SignalEnded,
            ))
            .unwrap(),
        ];
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/LIVE-STATE.md");
        let doc = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
            .replace("\r\n", "\n");
        for sample in samples {
            let kind = sample["kind"].as_str().unwrap().to_string();
            let mut serialized = std::collections::BTreeSet::new();
            keys(&sample, &mut serialized);
            let header = format!("## `{kind}` records");
            let section: Vec<&str> = doc
                .lines()
                .skip_while(|l| l.trim() != header)
                .skip(1)
                .take_while(|l| !l.starts_with("## "))
                .collect();
            assert!(
                !section.is_empty(),
                "docs/LIVE-STATE.md has no `{header}` section"
            );
            let rows: std::collections::BTreeSet<String> = section
                .iter()
                .filter_map(|l| l.strip_prefix("| `"))
                .filter_map(|l| l.split_once("` |"))
                .map(|(field, _)| field.to_string())
                .collect();
            for key in &serialized {
                assert!(
                    rows.contains(key),
                    "docs/LIVE-STATE.md `{kind}` section has no `| `{key}` |` row"
                );
            }
            for row in &rows {
                assert!(
                    serialized.contains(row),
                    "docs/LIVE-STATE.md documents `{row}` under `{kind}`, which is not serialized"
                );
            }
        }
    }
}
