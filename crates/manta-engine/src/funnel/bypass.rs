//! MAN-198: oracle-bypass experiments. Each forces one intervention with
//! truth the production pipeline does not have, reruns the stages after it,
//! and reports an upper bound for that one intervention. Side paths only:
//! no production code path changes, and nothing here is a recall score.
//! Every line carries `oracleAssisted: true` and is written after the
//! production summary (plan P12, P13).

use super::attribute::{
    attribute, DecisionFact, Evidence, Geometry, Opportunity, Record, SpotFact, Stage, Tolerances,
    TrackFacts,
};
use super::collect::{observe, spot_type_label, EventTrack};
use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::decoder::TrackDecoder;
use manta_decode::events::{ClosureKind, DecoderEvent};
use num_complex::Complex32;
use serde::Serialize;
use std::collections::BTreeMap;

/// One oracle-bypass experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bypass {
    /// A decoder leased on the truth frequency over the acoustic window,
    /// fed the strongest of the three channels around it (the oracle's own
    /// channel selection), then the production validator.
    KnownFrequency,
}

impl Bypass {
    /// The report's camelCase name.
    pub fn label(self) -> &'static str {
        match self {
            Bypass::KnownFrequency => "knownFrequency",
        }
    }

    /// The stages the bypass forces to pass.
    pub fn forced_stages(self) -> &'static [Stage] {
        match self {
            Bypass::KnownFrequency => &[Stage::DetectableEnergy, Stage::AdmittedTrack],
        }
    }
}

/// One `lossFunnelBypass` line. Deliberately no `dxCall`, `frequency` or
/// `spots` key, so no spot reader can take it for a spot.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BypassRecord {
    pub kind: &'static str,
    pub bypass: &'static str,
    pub oracle_assisted: bool,
    pub forced_stages: Vec<Stage>,
    pub call: String,
    pub frequency_hz: f64,
    pub first_s: f64,
    /// False when the bypass cannot run for this opportunity; `detail`
    /// then names why and `reached`/`lostAt` stay `null`.
    pub available: bool,
    pub reached: Option<Stage>,
    pub lost_at: Option<Stage>,
    pub detail: Option<String>,
    pub production_lost_at: Option<Stage>,
    pub speed_wpm: Option<f32>,
    pub track_text: Option<String>,
    pub decisions: Vec<String>,
}

/// One `lossFunnelBypassSummary` line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BypassSummary {
    pub kind: &'static str,
    pub bypass: &'static str,
    pub oracle_assisted: bool,
    pub opportunities: usize,
    pub available: usize,
    /// Unavailable opportunities by reason.
    pub unavailable: BTreeMap<String, usize>,
    /// Available opportunities the bypass delivered.
    pub delivered: usize,
    /// Delivered by the bypass but not by production.
    pub recovered_over_production: usize,
    /// Delivered by production or by the bypass. Not a recall score.
    pub upper_bound: usize,
    /// The bypass's earliest failed stage, over available opportunities.
    pub lost_at: BTreeMap<&'static str, usize>,
}

/// The production record's `inputCoverage` failures a bypass cannot fix:
/// there is no audio for the opportunity.
fn unavailable_reason(production: &Record) -> Option<&str> {
    match (production.lost_at, production.detail.as_deref()) {
        (Some(Stage::InputCoverage), Some(d @ ("outsideRecording" | "outsidePassband"))) => Some(d),
        _ => None,
    }
}

/// Runs `bypass` for every production record, in the records' order.
/// `iq` is the whole recording, sampled at `geo.sample_rate_hz` around the
/// uncalibrated `center_hz`; `geo` and `tol` are the production run's.
pub fn run_bypass(
    bypass: Bypass,
    iq: &[Complex32],
    center_hz: f64,
    cfg: &PipelineConfig,
    production: &[Record],
    geo: &Geometry,
    tol: &Tolerances,
) -> Result<(Vec<BypassRecord>, BypassSummary)> {
    let factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut records = Vec::with_capacity(production.len());
    for prod in production {
        let mut line = BypassRecord {
            kind: "lossFunnelBypass",
            bypass: bypass.label(),
            oracle_assisted: true,
            forced_stages: bypass.forced_stages().to_vec(),
            call: prod.call.clone(),
            frequency_hz: prod.frequency_hz,
            first_s: prod.first_s,
            available: false,
            reached: None,
            lost_at: None,
            detail: None,
            production_lost_at: prod.lost_at,
            speed_wpm: None,
            track_text: None,
            decisions: Vec::new(),
        };
        let covered = match (unavailable_reason(prod), prod.covered_s) {
            (None, Some(c)) => c,
            (reason, _) => {
                line.detail = Some(reason.unwrap_or("outsideRecording").to_string());
                records.push(line);
                continue;
            }
        };
        let op = Opportunity {
            call: prod.call.clone(),
            freq_hz: prod.frequency_hz,
            first_s: prod.first_s,
            last_s: prod.last_s,
            rows: prod.rows,
            snr_db: prod.snr_db,
            wpm: prod.wpm,
        };
        let ev = known_frequency_evidence(iq, center_hz, factor, cfg, geo, &op, covered)?;
        let r = attribute(&op, &ev, geo, tol);
        line.available = true;
        line.reached = r.reached;
        line.lost_at = r.lost_at;
        line.detail = r.detail;
        line.speed_wpm = r.speed_wpm;
        line.track_text = r.track_text;
        line.decisions = r.decisions;
        records.push(line);
    }
    let summary = summarize_bypass(bypass, &records);
    Ok((records, summary))
}

/// Decodes the truth frequency's channel over `covered` with a fresh
/// decoder and validator, and returns that one track's evidence.
fn known_frequency_evidence(
    iq: &[Complex32],
    center_hz: f64,
    factor: f64,
    cfg: &PipelineConfig,
    geo: &Geometry,
    op: &Opportunity,
    covered: [f64; 2],
) -> Result<Evidence> {
    const TRACK_ID: u32 = 1;
    let fs = geo.sample_rate_hz;
    let raw_freq_hz = op.freq_hz / factor;
    let mut ch =
        manta_dsp::channelizer::Channelizer::new(fs, center_hz).map_err(anyhow::Error::msg)?;
    let n = ch.n_channels();
    let delta = fs / n as f64;
    let k0 = (((raw_freq_hz - center_hz) / delta).round() as i64).rem_euclid(n as i64) as usize;
    let s0 = ((covered[0] * fs) as usize).min(iq.len());
    let s1 = ((covered[1] * fs) as usize).clamp(s0, iq.len());
    // `manta oracle`'s rule (`manta_testkit::oracle::run_oracle`): the
    // channel with the highest mean power of the three around the truth.
    // Streamed one second at a time, keeping only those three channels, so
    // a long episode does not hold every hop's full spectrum.
    let cands = [(k0 + n - 1) % n, k0, (k0 + 1) % n];
    let mut powers: Vec<[f32; 3]> = Vec::new();
    for chunk in iq[s0..s1].chunks((fs as usize).max(1)) {
        for h in ch.process(chunk) {
            powers.push(cands.map(|k| h.power[k]));
        }
    }
    let mut sums = [0.0f64; 3];
    for p in &powers {
        for (s, &v) in sums.iter_mut().zip(p) {
            *s += f64::from(v);
        }
    }
    let own = (0..3)
        .max_by(|&a, &b| sums[a].total_cmp(&sums[b]))
        .unwrap_or(1);
    let mut dec = TrackDecoder::new(TRACK_ID, cfg.decode.clone());
    dec.set_freq_hz(raw_freq_hz);
    // A placeholder: bypass lines carry no SNR.
    dec.set_snr_2500_db(0.0);
    let mut events = vec![DecoderEvent::TrackPromoted {
        track_id: TRACK_ID,
        sample_ts: s0 as u64,
        freq_hz: raw_freq_hz,
    }];
    for (i, p) in powers.iter().enumerate() {
        let p = p[own];
        events.extend(dec.push_hop(p.sqrt(), p, None, (s0 + i * ch.hop()) as u64));
    }
    events.extend(dec.finish());
    events.push(DecoderEvent::TrackClosed {
        track_id: TRACK_ID,
        closure: ClosureKind::SignalEnded,
    });

    let mut validator = cfg.validator(fs)?;
    validator.record_decisions(true);
    let mut tracks: BTreeMap<u32, EventTrack> = BTreeMap::new();
    let mut decisions = Vec::new();
    let mut spots = Vec::new();
    for e in &events {
        observe(&mut tracks, e);
        spots.extend(validator.ingest(e));
        decisions.extend(validator.take_decisions());
    }
    spots.extend(validator.finish_lookups());
    decisions.extend(validator.take_decisions());

    let secs = |ts: u64| ts as f64 / fs;
    let t = tracks.remove(&TRACK_ID).unwrap_or_default();
    let track = TrackFacts {
        track_id: TRACK_ID,
        spawn_freq_hz: op.freq_hz,
        freq_hz: op.freq_hz,
        born_s: covered[0],
        end_s: Some(covered[1]),
        confirmed: true,
        leased: true,
        close: None,
        speeds: t.speeds.iter().map(|&(ts, w)| (secs(ts), w)).collect(),
        text: t.text.trim().to_string(),
        readings: t.readings,
    };
    Ok(Evidence {
        tracks: vec![track],
        decisions: decisions
            .into_iter()
            .map(|d| DecisionFact {
                call: d.call,
                freq_hz: d.freq_hz,
                t_s: secs(d.sample_ts),
                reason: d.reason,
                reps: d.reps,
                required_reps: d.required_reps,
            })
            .collect(),
        spots: spots
            .into_iter()
            .map(|s| SpotFact {
                t_s: secs(s.sample_ts),
                spot_type: spot_type_label(s.spot_type).to_string(),
                call: s.callsign,
                freq_hz: s.freq_hz,
                track_id: s.track_id,
                sample_ts: s.sample_ts,
            })
            .collect(),
        vetoes: Vec::new(),
    })
}

/// Totals one bypass's lines. `upperBound` counts an opportunity delivered
/// by production or by the bypass; neither number is a recall score.
pub fn summarize_bypass(bypass: Bypass, records: &[BypassRecord]) -> BypassSummary {
    let mut s = BypassSummary {
        kind: "lossFunnelBypassSummary",
        bypass: bypass.label(),
        oracle_assisted: true,
        opportunities: records.len(),
        available: 0,
        unavailable: BTreeMap::new(),
        delivered: 0,
        recovered_over_production: 0,
        upper_bound: 0,
        lost_at: BTreeMap::new(),
    };
    for r in records {
        let production_delivered = r.production_lost_at.is_none();
        if !r.available {
            *s.unavailable
                .entry(r.detail.clone().unwrap_or_default())
                .or_insert(0) += 1;
            s.upper_bound += usize::from(production_delivered);
            continue;
        }
        s.available += 1;
        match r.lost_at {
            None => {
                s.delivered += 1;
                s.recovered_over_production += usize::from(!production_delivered);
                s.upper_bound += 1;
            }
            Some(st) => {
                *s.lost_at.entry(st.as_str()).or_insert(0) += 1;
                s.upper_bound += usize::from(production_delivered);
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        available: bool,
        lost_at: Option<Stage>,
        production_lost_at: Option<Stage>,
    ) -> BypassRecord {
        BypassRecord {
            kind: "lossFunnelBypass",
            bypass: "knownFrequency",
            oracle_assisted: true,
            forced_stages: Bypass::KnownFrequency.forced_stages().to_vec(),
            call: "W1AW".into(),
            frequency_hz: 14_012_300.0,
            first_s: 25.0,
            available,
            reached: None,
            lost_at,
            detail: (!available).then(|| "outsidePassband".to_string()),
            production_lost_at,
            speed_wpm: None,
            track_text: None,
            decisions: Vec::new(),
        }
    }

    #[test]
    fn bypass_summary_bounds_production() {
        let records = [
            // Delivered by both.
            line(true, None, None),
            // Recovered by the bypass only.
            line(true, None, Some(Stage::DetectableEnergy)),
            // Lost by both, at different stages.
            line(
                true,
                Some(Stage::UsableTiming),
                Some(Stage::DetectableEnergy),
            ),
            // Production delivered, the bypass lost it: still bounded.
            line(true, Some(Stage::SufficientEvidence), None),
            // No audio.
            line(false, None, Some(Stage::InputCoverage)),
        ];
        let s = summarize_bypass(Bypass::KnownFrequency, &records);
        assert!(s.oracle_assisted);
        assert_eq!(s.opportunities, 5);
        assert_eq!(s.available, 4);
        assert_eq!(s.unavailable.get("outsidePassband"), Some(&1));
        assert_eq!(s.delivered, 2);
        assert_eq!(s.recovered_over_production, 1);
        assert_eq!(s.upper_bound, 3);
        assert_eq!(s.lost_at.get("usableTiming"), Some(&1));
        assert_eq!(s.lost_at.get("sufficientEvidence"), Some(&1));
    }

    #[test]
    fn bypass_lines_carry_no_spot_keys() {
        let v = serde_json::to_value(line(true, None, None)).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o["oracleAssisted"], serde_json::json!(true));
        assert_eq!(
            o["forcedStages"],
            serde_json::json!(["detectableEnergy", "admittedTrack"])
        );
        for key in ["dxCall", "frequency", "spots"] {
            assert!(!o.contains_key(key), "{key}");
        }
    }
}
