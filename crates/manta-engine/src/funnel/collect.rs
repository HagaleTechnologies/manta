//! MAN-198: one production pass with the funnel's observers attached, and
//! the evidence it leaves.

use super::attribute::{
    attribute, summarize, Births, CloseTally, DecisionFact, Evidence, Geometry, Opportunity,
    Record, SpotFact, Summary, Tolerances, TrackFacts, VetoFact,
};
use super::trace::{FunnelTrace, TraceLog, TrackTraceRecord};
use crate::live::{LiveRecord, LiveStateSink};
use crate::{ListenObservers, PipelineConfig, SpotType};
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// What `run_funnel` found: one record per opportunity, in (call,
/// frequency, first report) order, and their summary.
#[derive(Debug, Clone, PartialEq)]
pub struct FunnelReport {
    pub geometry: Geometry,
    pub records: Vec<Record>,
    pub summary: Summary,
}

/// Collects every validator decision the live-state tap publishes.
#[derive(Default)]
struct DecisionLog(Mutex<Vec<manta_spot::Decision>>);

impl LiveStateSink for DecisionLog {
    fn wants_records(&self) -> bool {
        true
    }

    fn publish(&self, record: LiveRecord) {
        if let LiveRecord::Decision(d) = record {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(d);
        }
    }
}

/// Per-track state from the event stream.
#[derive(Default)]
pub(super) struct EventTrack {
    last_ts: u64,
    promoted_freq_hz: Option<f64>,
    meta_freq_hz: Option<f64>,
    pub(super) speeds: Vec<(u64, f32)>,
    pub(super) text: String,
    pub(super) readings: Vec<String>,
}

fn event_track(e: &DecoderEvent) -> (u32, Option<u64>) {
    match e {
        DecoderEvent::ReadingsHeld {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::WordBoundary {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::TrackMeta {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::TimingUpdate {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::TrackPromoted {
            track_id,
            sample_ts,
            ..
        } => (*track_id, Some(*sample_ts)),
        DecoderEvent::SpeedUpdate { track_id, .. } | DecoderEvent::TrackClosed { track_id, .. } => {
            (*track_id, None)
        }
    }
}

pub(super) fn observe(tracks: &mut BTreeMap<u32, EventTrack>, e: &DecoderEvent) {
    let (id, ts) = event_track(e);
    let t = tracks.entry(id).or_default();
    if let Some(ts) = ts {
        t.last_ts = t.last_ts.max(ts);
    }
    match e {
        DecoderEvent::CharDecoded { glyph, .. } => {
            if let Some(c) = glyph.text_char() {
                t.text.push(c);
            }
        }
        DecoderEvent::WordBoundary { .. } if !t.text.is_empty() && !t.text.ends_with(' ') => {
            t.text.push(' ');
        }
        DecoderEvent::ReadingsHeld { readings, .. } => {
            for (word, _) in readings {
                if !t.readings.contains(word) {
                    t.readings.push(word.clone());
                }
            }
        }
        // `SpeedUpdate` carries no time: stamp it with the track's latest.
        DecoderEvent::SpeedUpdate { wpm, .. } => t.speeds.push((t.last_ts, *wpm)),
        DecoderEvent::TrackMeta { freq_hz, .. } => t.meta_freq_hz = Some(*freq_hz),
        DecoderEvent::TrackPromoted { freq_hz, .. } => t.promoted_freq_hz = Some(*freq_hz),
        _ => {}
    }
}

pub(super) fn spot_type_label(t: SpotType) -> &'static str {
    match t {
        SpotType::Cq => "cq",
        SpotType::De => "de",
        SpotType::Beacon => "beacon",
        SpotType::Unknown => "unknown",
    }
}

/// Builds the funnel's evidence from a run's trace, events, decisions and
/// spots. Frequencies come out calibrated by `factor`, times in seconds.
fn evidence(
    trace: TraceLog,
    events: BTreeMap<u32, EventTrack>,
    decisions: Vec<manta_spot::Decision>,
    spots: Vec<crate::Spot>,
    fs: f64,
    factor: f64,
) -> (Evidence, Births, CloseTally) {
    let secs = |ts: u64| ts as f64 / fs;
    let mut births = Births::default();
    let mut closes = CloseTally::default();
    let mut tracks: BTreeMap<u32, TrackFacts> = BTreeMap::new();
    let TraceLog { records, vetoes } = trace;
    for r in records {
        match r {
            TrackTraceRecord::Spawned {
                track_id,
                freq_hz,
                sample_ts,
            } => {
                births.candidates += 1;
                let f = freq_hz * factor;
                tracks.insert(
                    track_id,
                    TrackFacts {
                        track_id,
                        spawn_freq_hz: f,
                        freq_hz: f,
                        born_s: secs(sample_ts),
                        end_s: None,
                        confirmed: false,
                        leased: false,
                        close: None,
                        speeds: Vec::new(),
                        text: String::new(),
                        readings: Vec::new(),
                    },
                );
            }
            TrackTraceRecord::Confirmed { track_id, .. } => {
                births.confirmed += 1;
                if let Some(t) = tracks.get_mut(&track_id) {
                    t.confirmed = true;
                }
            }
            TrackTraceRecord::Closed {
                track_id,
                sample_ts,
                reason,
            } => {
                closes.record(reason);
                if let Some(t) = tracks.get_mut(&track_id) {
                    t.end_s = Some(secs(sample_ts));
                    t.close = Some(reason);
                }
            }
        }
    }
    for (id, e) in events {
        let Some(t) = tracks.get_mut(&id) else {
            continue;
        };
        if let Some(f) = e.promoted_freq_hz {
            births.leased += 1;
            t.leased = true;
            t.freq_hz = f;
        }
        if let Some(f) = e.meta_freq_hz {
            t.freq_hz = f;
        }
        t.speeds = e.speeds.iter().map(|&(ts, w)| (secs(ts), w)).collect();
        t.text = e.text.trim().to_string();
        t.readings = e.readings;
    }
    closes.open_at_end = tracks.values().filter(|t| t.close.is_none()).count();
    let decisions = decisions
        .into_iter()
        .map(|d| DecisionFact {
            call: d.call,
            freq_hz: d.freq_hz,
            t_s: secs(d.sample_ts),
            reason: d.reason,
            reps: d.reps,
            required_reps: d.required_reps,
        })
        .collect();
    let spots = spots
        .into_iter()
        .map(|s| SpotFact {
            t_s: secs(s.sample_ts),
            spot_type: spot_type_label(s.spot_type).to_string(),
            call: s.callsign,
            freq_hz: s.freq_hz,
            track_id: s.track_id,
            sample_ts: s.sample_ts,
        })
        .collect();
    (
        Evidence {
            tracks: tracks.into_values().collect(),
            decisions,
            spots,
            vetoes: vetoes
                .into_iter()
                .map(|((second, centi_hz, veto), count)| VetoFact {
                    t_s: second as f64,
                    freq_hz: centi_hz as f64 / 100.0 * factor,
                    veto,
                    count,
                })
                .collect(),
        },
        births,
        closes,
    )
}

/// Runs `src` once through the production pipeline (`listen_with_observers`)
/// with the funnel's write-only observers attached, then charges every
/// opportunity to one stage. `duration_s` is the recording's length. The
/// spots are those `decode_source` would emit; nothing here changes them.
pub fn run_funnel(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    opportunities: &[Opportunity],
    tol: &Tolerances,
    duration_s: f64,
) -> Result<FunnelReport> {
    let factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    // Relative to the centre, as `IqSource::rf_passband_hz` reports it.
    let passband = src.rf_passband_hz();
    let n_channels = manta_dsp::channelizer::Channelizer::new(fs, center)
        .map_err(|e| anyhow::anyhow!(e))?
        .n_channels();
    let geometry = Geometry {
        sample_rate_hz: fs,
        center_freq_hz: center * factor,
        passband_hz: (
            (center + passband.0) * factor,
            (center + passband.1) * factor,
        ),
        channel_width_hz: fs / n_channels as f64,
        duration_s,
        freq_correction_ppm: cfg.freq_correction_ppm,
    };
    let trace = FunnelTrace::new();
    let decisions = Arc::new(DecisionLog::default());
    let observers = ListenObservers {
        funnel_trace: Some(trace.clone()),
        live_state: Some(decisions.clone()),
        ..Default::default()
    };
    let mut events: BTreeMap<u32, EventTrack> = BTreeMap::new();
    let mut spots: Vec<crate::Spot> = Vec::new();
    crate::listen_with_observers(
        src,
        cfg,
        Arc::new(AtomicBool::new(false)),
        observers,
        |e| observe(&mut events, e),
        |s| spots.push(s.clone()),
        |_| {},
    )?;
    let decisions = std::mem::take(&mut *decisions.0.lock().unwrap_or_else(|e| e.into_inner()));
    let (ev, births, closes) = evidence(trace.take(), events, decisions, spots, fs, factor);
    let mut records: Vec<Record> = opportunities
        .iter()
        .map(|op| attribute(op, &ev, &geometry, tol))
        .collect();
    records.sort_by(|a, b| {
        a.call
            .cmp(&b.call)
            .then(
                ((a.frequency_hz / 100.0).round() as i64)
                    .cmp(&((b.frequency_hz / 100.0).round() as i64)),
            )
            .then(a.first_s.total_cmp(&b.first_s))
    });
    let summary = summarize(&records, births, closes);
    Ok(FunnelReport {
        geometry,
        records,
        summary,
    })
}
