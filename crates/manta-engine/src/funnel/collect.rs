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
    /// (first character, last character, word), in decode order.
    pub(super) words: Vec<(u64, u64, String)>,
    /// (first holding, last holding, word): one entry per run of
    /// consecutive `ReadingsHeld` events that held the word, with no
    /// character committed and no word boundary between them.
    pub(super) readings: Vec<(u64, u64, String)>,
    /// `readings` indices the latest `ReadingsHeld` event held, cleared
    /// when a run ends.
    held: Vec<usize>,
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
    // A held reading lives within one word: a committed character or a
    // word boundary ends every run, so a later holding starts a new one
    // however long the unambiguous text between them.
    if matches!(
        e,
        DecoderEvent::CharDecoded { .. } | DecoderEvent::WordBoundary { .. }
    ) {
        t.held.clear();
    }
    match e {
        DecoderEvent::CharDecoded {
            glyph, sample_ts, ..
        } => {
            if let Some(c) = glyph.text_char() {
                let starts_word = t.text.is_empty() || t.text.ends_with(' ');
                t.text.push(c);
                match t.words.last_mut() {
                    Some((first, last, word)) if !starts_word => {
                        *first = (*first).min(*sample_ts);
                        *last = (*last).max(*sample_ts);
                        word.push(c);
                    }
                    _ => t.words.push((*sample_ts, *sample_ts, c.to_string())),
                }
            }
        }
        DecoderEvent::WordBoundary { .. } if !t.text.is_empty() && !t.text.ends_with(' ') => {
            t.text.push(' ');
        }
        DecoderEvent::ReadingsHeld {
            readings,
            sample_ts,
            ..
        } => {
            // A word the previous event also held extends that run; any
            // other word starts a new one, so each run keeps its own time.
            let previous = std::mem::take(&mut t.held);
            for (word, _) in readings {
                let run = t
                    .held
                    .iter()
                    .chain(&previous)
                    .copied()
                    .find(|&i| t.readings[i].2 == *word);
                let i = match run {
                    Some(i) => {
                        let (first, last, _) = &mut t.readings[i];
                        *first = (*first).min(*sample_ts);
                        *last = (*last).max(*sample_ts);
                        i
                    }
                    None => {
                        t.readings.push((*sample_ts, *sample_ts, word.clone()));
                        t.readings.len() - 1
                    }
                };
                if !t.held.contains(&i) {
                    t.held.push(i);
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

/// `(first, last, word)` spans in seconds.
pub(super) fn timed(
    spans: Vec<(u64, u64, String)>,
    secs: impl Fn(u64) -> f64,
) -> Vec<(f64, f64, String)> {
    spans
        .into_iter()
        .map(|(first, last, word)| (secs(first), secs(last), word))
        .collect()
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
                        words: Vec::new(),
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
        t.words = timed(e.words, secs);
        t.readings = timed(e.readings, secs);
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

#[cfg(test)]
mod tests {
    use super::*;
    use manta_decode::tree::Glyph;

    #[test]
    fn observe_times_each_word_and_each_run_of_held_readings() {
        let ch = |ts, c| DecoderEvent::char_decoded(1, ts, Glyph::Char(c), 1.0);
        let held = |ts, words: &[&str]| DecoderEvent::ReadingsHeld {
            track_id: 1,
            sample_ts: ts,
            readings: words.iter().map(|w| (w.to_string(), 0.0)).collect(),
            settled: false,
        };
        let mut tracks = BTreeMap::new();
        // Prior-on order: snapshots while a word is held, then the settled
        // characters, then the boundary.
        for e in [
            held(10, &["W", "M"]),
            held(20, &["W1", "M1"]),
            // Held again in the next event: the same run, held to 25.
            held(25, &["W1", "W1"]),
            ch(10, 'W'),
            ch(20, '1'),
            DecoderEvent::word_boundary(1, 30),
            DecoderEvent::word_boundary(1, 35),
            ch(40, 'K'),
            ch(400, 'K'),
            DecoderEvent::word_boundary(1, 410),
            // Held again only after committed text: a new run with its
            // own time, not one spanning 20 to 500.
            held(500, &["W1"]),
        ] {
            observe(&mut tracks, &e);
        }
        let t = &tracks[&1];
        assert_eq!(t.text, "W1 KK ");
        let span = |a, b, w: &str| (a, b, w.to_string());
        assert_eq!(t.words, vec![span(10, 20, "W1"), span(40, 400, "KK")]);
        assert_eq!(
            t.readings,
            vec![
                span(10, 10, "W"),
                span(10, 10, "M"),
                span(20, 25, "W1"),
                span(20, 20, "M1"),
                span(500, 500, "W1"),
            ]
        );
    }
}
