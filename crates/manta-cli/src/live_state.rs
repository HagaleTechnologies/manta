//! MAN-324: the daemon's `manta_engine::LiveStateSink`. Maps each engine
//! live record onto its `manta_server::live_state` wire record, serialises
//! it once on the decode thread and hands the line to the bus's live
//! channel, which `json_stream::serve` (`StreamKind::Live`) fans out. The
//! wiring layer, like `decode_quality_of`: `manta-server` cannot see
//! `manta-engine`'s types.

use manta_decode::events::ClosureKind;
use manta_engine::{LiveRecord, LiveStateSink};
use manta_server::bus::SpotBus;
use manta_server::live_state::{
    band_rollup, BandInput, BandStateRecord, CandidateDecisionRecord, CandidateRow, ChannelLevel,
    TrackClosedRecord, TrackClosure, TrackStateInput, TrackStateRecord, BAND_STATE_KIND,
    CANDIDATE_DECISION_KIND, TRACK_CLOSED_KIND, TRACK_STATE_KIND,
};
use manta_server::metrics::Metrics;
use std::sync::Arc;

pub(crate) struct LiveSink {
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    station_callsign: String,
    /// MAN-13: with several inputs, their names in config order; each
    /// `bandState` then names its input (`BandSample::input`).
    sources: Option<Vec<String>>,
}

impl LiveSink {
    pub(crate) fn new(bus: Arc<SpotBus>, metrics: Arc<Metrics>, station_callsign: &str) -> Self {
        Self {
            bus,
            metrics,
            station_callsign: station_callsign.to_string(),
            sources: None,
        }
    }

    /// MAN-13: a sink for several inputs, named in config order.
    pub(crate) fn with_sources(mut self, names: &[String]) -> Self {
        self.sources = Some(names.to_vec());
        self
    }

    /// The input name a `bandState` carries: only with several inputs.
    fn source_of(&self, record: &LiveRecord) -> Option<&str> {
        match (record, &self.sources) {
            (LiveRecord::Bands(b), Some(names)) => names.get(b.input).map(String::as_str),
            _ => None,
        }
    }
}

impl LiveStateSink for LiveSink {
    /// Records are built only while someone is subscribed.
    fn wants_records(&self) -> bool {
        self.bus.live_receiver_count() > 0
    }

    /// A non-blocking broadcast send; a slow subscriber lags and is
    /// disconnected by its own handler, never waited on here.
    fn publish(&self, record: LiveRecord) {
        let source = self.source_of(&record);
        let (kind, line) = render_from(&record, &self.station_callsign, source, &self.bus);
        if self.bus.publish_live(Arc::from(line)) {
            self.metrics.record_live_published(kind);
        }
    }
}

/// One engine live record as its `kind` and serialised wire line.
#[cfg(test)]
pub(crate) fn render(record: &LiveRecord, de_call: &str, bus: &SpotBus) -> (&'static str, String) {
    render_from(record, de_call, None, bus)
}

/// [`render`], naming the input a `bandState` describes when `source` is
/// set (MAN-13).
pub(crate) fn render_from(
    record: &LiveRecord,
    de_call: &str,
    source: Option<&str>,
    bus: &SpotBus,
) -> (&'static str, String) {
    match record {
        LiveRecord::Bands(b) => {
            let channels: Vec<ChannelLevel> = b
                .channels
                .iter()
                .map(|c| ChannelLevel {
                    freq_hz: c.freq_hz,
                    floor_db: c.floor_db,
                    occupied: c.occupied,
                })
                .collect();
            let bands = band_rollup(BandInput {
                channels: &channels,
                decoding_track_freqs_hz: &b.decoding_track_freqs_hz,
            });
            let mut rec = BandStateRecord::new(de_call, bus.unix_ts_for(b.sample_ts), bands);
            if let Some(source) = source {
                rec = rec.with_source(source);
            }
            (BAND_STATE_KIND, to_line(&rec))
        }
        LiveRecord::Track(t) => {
            let rec = TrackStateRecord::new(
                de_call,
                bus.unix_ts_for(t.sample_ts),
                TrackStateInput {
                    track_id: t.track_id,
                    freq_hz: t.freq_hz,
                    wpm: t.wpm,
                    snr_2500_db: t.snr_2500_db,
                    text: &t.text,
                    candidates: t
                        .candidates
                        .iter()
                        .map(|c| CandidateRow::new(&c.call, &c.support))
                        .collect(),
                },
            );
            (TRACK_STATE_KIND, to_line(&rec))
        }
        LiveRecord::Decision(d) => {
            // The same id the `:7301` stream gives this spot, so a
            // dashboard can join the two.
            let spot_id = d
                .spot
                .as_ref()
                .map(|s| manta_server::spot_message::spot_id(de_call, bus.session_nonce(), s));
            let rec =
                CandidateDecisionRecord::new(de_call, bus.unix_ts_for(d.sample_ts), d, spot_id);
            (CANDIDATE_DECISION_KIND, to_line(&rec))
        }
        LiveRecord::TrackClosed {
            track_id,
            sample_ts,
            closure,
        } => {
            let closure = match closure {
                ClosureKind::SignalEnded => TrackClosure::SignalEnded,
                ClosureKind::Bookkeeping { survivor_track_id } => {
                    TrackClosure::Bookkeeping(*survivor_track_id)
                }
            };
            let rec =
                TrackClosedRecord::new(de_call, bus.unix_ts_for(*sample_ts), *track_id, closure);
            (TRACK_CLOSED_KIND, to_line(&rec))
        }
    }
}

fn to_line(record: &impl serde::Serialize) -> String {
    serde_json::to_string(record).expect("live-state records always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_engine::{BandSample, ChannelLevel as EngineChannel, TrackSnapshot};
    use manta_spot::{CandidateSupport, Decision, Reason, Spot, SpotType, Support, Verdict};
    use std::time::{Duration, SystemTime};

    const FS: f64 = 96_000.0;
    const NONCE: u128 = 12_534_119_743_695_398_054;

    fn bus() -> Arc<SpotBus> {
        Arc::new(SpotBus::new(
            FS,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            NONCE,
        ))
    }

    fn spot() -> Spot {
        Spot {
            callsign: "W1AW".into(),
            freq_hz: 14_012_362.0,
            snr_db: 21.0,
            wpm: 20.0,
            spot_type: SpotType::Cq,
            confidence: 0.9,
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 2_006_272,
            rst: None,
            qrl_query: false,
        }
    }

    #[test]
    fn band_state_names_its_input_with_several_inputs() {
        let sample = |input| {
            LiveRecord::Bands(BandSample {
                input,
                sample_ts: 5 * FS as u64,
                channels: vec![],
                decoding_track_freqs_hz: vec![],
            })
        };
        let metrics = Arc::new(Metrics::new());
        let one = LiveSink::new(bus(), metrics.clone(), "W1AW");
        assert_eq!(one.source_of(&sample(0)), None);
        let names = ["20m".to_string(), "40m".to_string()];
        let several = LiveSink::new(bus(), metrics, "W1AW").with_sources(&names);
        assert_eq!(several.source_of(&sample(1)), Some("40m"));
        let (_, line) = render_from(&sample(1), "W1AW", Some("40m"), &bus());
        assert_eq!(
            line,
            r#"{"kind":"bandState","deCall":"W1AW","source":"40m","timestamp":1700000005,"bands":[]}"#
        );
    }

    #[test]
    fn each_live_record_maps_to_its_documented_line() {
        let bus = bus();
        let bands = LiveRecord::Bands(BandSample {
            input: 0,
            sample_ts: 5 * FS as u64,
            channels: vec![
                EngineChannel {
                    freq_hz: 13_999_906.25,
                    floor_db: -62.2,
                    occupied: false,
                },
                EngineChannel {
                    freq_hz: 14_000_000.0,
                    floor_db: -62.2,
                    occupied: true,
                },
            ],
            decoding_track_freqs_hz: vec![14_012_362.0],
        });
        assert_eq!(
            render(&bands, "W1AW", &bus),
            (
                "bandState",
                r#"{"kind":"bandState","deCall":"W1AW","timestamp":1700000005,"bands":[{"band":"20m","loHz":14000000,"hiHz":14000000,"channels":1,"noiseFloorDb":-62.2,"occupancy":1.0,"activeTracks":1},{"band":"unknown","loHz":13999906,"hiHz":13999906,"channels":1,"noiseFloorDb":-62.2,"occupancy":0.0,"activeTracks":0}]}"#.to_string()
            )
        );
        let track = LiveRecord::Track(TrackSnapshot {
            track_id: 1,
            sample_ts: 13 * FS as u64,
            freq_hz: Some(14_012_362.0),
            wpm: Some(20.0),
            snr_2500_db: Some(21.0),
            text: "CQ CQ DE W1AW".into(),
            candidates: vec![CandidateSupport {
                call: "W1AW".into(),
                support: Support {
                    reps: 1,
                    conf_sum: 0.55,
                },
            }],
        });
        assert_eq!(
            render(&track, "W1AW", &bus),
            (
                "trackState",
                r#"{"kind":"trackState","deCall":"W1AW","timestamp":1700000013,"trackId":1,"frequency":14012362,"band":"20m","wpm":20,"snr":21,"snrRefHz":2500,"text":"CQ CQ DE W1AW","candidates":[{"call":"W1AW","reps":1,"confSum":0.55}]}"#.to_string()
            )
        );
        let withheld = LiveRecord::Decision(Decision {
            track_id: 1,
            sample_ts: 13 * FS as u64,
            call: "W1AW".into(),
            freq_hz: 14_012_362.0,
            spot_type: SpotType::Cq,
            verdict: Verdict::Withheld,
            reason: Some(Reason::Repetition),
            reps: Some(1),
            required_reps: 2,
            pattern_tier: Some(manta_spot::PatternTier::Common),
            rival: None,
            confidence: None,
            spot: None,
        });
        assert_eq!(
            render(&withheld, "W1AW", &bus),
            (
                "candidateDecision",
                r#"{"kind":"candidateDecision","deCall":"W1AW","timestamp":1700000013,"trackId":1,"call":"W1AW","frequency":14012362,"band":"20m","spotType":"cq","decision":"withheld","reason":"repetition","reps":1,"requiredReps":2,"patternTier":"common","rival":null,"spotId":null}"#.to_string()
            )
        );
        let closed = LiveRecord::TrackClosed {
            track_id: 1,
            sample_ts: 120 * FS as u64,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(4),
            },
        };
        assert_eq!(
            render(&closed, "W1AW", &bus),
            (
                "trackClosed",
                r#"{"kind":"trackClosed","deCall":"W1AW","timestamp":1700000120,"trackId":1,"closure":"bookkeeping","survivorTrackId":4}"#.to_string()
            )
        );
    }

    #[test]
    fn a_spotted_decisions_spot_id_equals_the_spot_streams_id() {
        let bus = bus();
        let s = spot();
        let spotted = LiveRecord::Decision(Decision {
            track_id: s.track_id,
            sample_ts: s.sample_ts,
            call: s.callsign.clone(),
            freq_hz: s.freq_hz,
            spot_type: s.spot_type,
            verdict: Verdict::Spotted,
            reason: None,
            reps: Some(2),
            required_reps: 2,
            pattern_tier: Some(manta_spot::PatternTier::Common),
            rival: None,
            confidence: Some(0.75),
            spot: Some(s.clone()),
        });
        let (_, line) = render(&spotted, "W1AW", &bus);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        let cty = manta_spot::cty::Table::parse(manta_spot::CTY_DAT);
        let msg = manta_server::spot_message::SpotMessage::from_spot(
            &s,
            "W1AW",
            None,
            &cty,
            "v",
            bus.unix_ts_for(s.sample_ts),
            bus.session_nonce(),
            0,
        );
        assert_eq!(value["spotId"], serde_json::Value::String(msg.id.clone()));
        assert_eq!(value["decision"], "spotted");
        assert_eq!(value["reason"], serde_json::Value::Null);
    }

    #[test]
    fn no_subscriber_means_wants_records_is_false() {
        let bus = bus();
        let metrics = Arc::new(Metrics::new());
        metrics.enable_live();
        let sink = LiveSink::new(bus.clone(), metrics.clone(), "W1AW");
        assert!(!sink.wants_records());
        // Published with no subscriber: dropped and not counted.
        sink.publish(LiveRecord::TrackClosed {
            track_id: 1,
            sample_ts: 0,
            closure: ClosureKind::SignalEnded,
        });
        assert!(metrics
            .render_prometheus_text()
            .contains("manta_live_records_total{kind=\"trackClosed\"} 0\n"));
        let mut rx = bus.subscribe_live();
        assert!(sink.wants_records());
        sink.publish(LiveRecord::TrackClosed {
            track_id: 1,
            sample_ts: 0,
            closure: ClosureKind::SignalEnded,
        });
        assert!(matches!(
            rx.try_recv(),
            Ok(manta_server::bus::JsonItem::Live(_))
        ));
        assert!(metrics
            .render_prometheus_text()
            .contains("manta_live_records_total{kind=\"trackClosed\"} 1\n"));
    }
}
