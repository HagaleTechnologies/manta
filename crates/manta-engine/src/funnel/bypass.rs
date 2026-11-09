//! MAN-198/MAN-493: oracle-bypass experiments. Each forces one
//! intervention with truth the production pipeline does not have, reruns
//! the stages after it, and reports an upper bound for that one
//! intervention. Side paths only: no production code path changes, and
//! nothing here is a recall score. Every line carries `oracleAssisted:
//! true` and is written after the production summary (plan P12, P13).
//!
//! Four experiments (MAN-493 built the last three): the two RBN-fed ones
//! (`known-frequency`, `known-timing`) run through [`run_bypass`] and are
//! CLI flag values; the two keying-truth ones (`known-transitions`,
//! `known-transcript`) run through [`run_synthetic_bypass`] and are
//! engine-API/example-only -- the CLI keeps rejecting their names.

use super::attribute::{
    attribute, DecisionFact, Evidence, Geometry, Opportunity, Record, SpotFact, Stage, Tolerances,
    TrackFacts,
};
use super::collect::{observe, spot_type_label, timed, EventTrack};
use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::decoder::TrackDecoder;
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
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
    /// `known-frequency` plus the truth speed seeded into the decoder
    /// (`TrackDecoder::seed_speed_wpm`), so the speed gate the production
    /// decode failed is forced by construction. Needs the truth layer's
    /// own WPM: an opportunity with none, or an hsmm engine (whose speed
    /// never passes through `SpeedTracker`), is unavailable.
    KnownTiming,
    /// The exact on/off keying timeline of the truth text, fed hop by hop
    /// to a fresh decoder (`TrackDecoder::push_envelope`): detection,
    /// admission and timing are forced by construction, everything from
    /// the text stages on is honestly evaluated. Synthetic truth; engine
    /// API and example only.
    KnownTransitions,
    /// The truth transcript synthesized straight into decoder events, so
    /// every decode stage is forced and only the validator's own gates
    /// (self-ID, evidence, policy) are honestly evaluated. Synthetic
    /// truth; engine API and example only.
    KnownTranscript,
}

impl Bypass {
    /// The report's camelCase name.
    pub fn label(self) -> &'static str {
        match self {
            Bypass::KnownFrequency => "knownFrequency",
            Bypass::KnownTiming => "knownTiming",
            Bypass::KnownTransitions => "knownTransitions",
            Bypass::KnownTranscript => "knownTranscript",
        }
    }

    /// The stages the bypass forces to pass.
    pub fn forced_stages(self) -> &'static [Stage] {
        match self {
            Bypass::KnownFrequency => &[Stage::DetectableEnergy, Stage::AdmittedTrack],
            Bypass::KnownTiming | Bypass::KnownTransitions | Bypass::KnownTranscript => &[
                Stage::DetectableEnergy,
                Stage::AdmittedTrack,
                Stage::UsableTiming,
            ],
        }
    }
}

/// The synthetic keying truth one synthetic bypass runs on
/// (`manta-testkit::funnel_truth` builds this as plain data; the engine
/// cannot name testkit types outside its dev-dependencies).
#[derive(Debug, Clone, Default)]
pub struct SyntheticTruth {
    /// The truth speed, WPM. Seeds the decoder for `KnownTransitions`
    /// and is reported for `KnownTranscript`.
    pub wpm: f32,
    /// The exact keying envelope at the decoder's hop rate: (input
    /// sample timestamp, amplitude 0..1). `KnownTransitions`.
    pub envelope: Vec<(u64, f32)>,
    /// The known words with each character's keyed span. `KnownTranscript`.
    pub words: Vec<TruthWord>,
}

/// One known word: its text and each character's keyed span, `(first
/// element start, last element end)` in input samples. The transcript
/// bypass emits each character at its real end, so a word's duration (and
/// the silence after it) is the truth's, not an approximation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TruthWord {
    pub word: String,
    pub chars: Vec<(u64, u64)>,
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

/// The same blank line every runner starts from: flagged oracle-assisted,
/// forcing its stages, unavailable until a runner fills it in.
fn blank_line(bypass: Bypass, prod: &Record) -> BypassRecord {
    BypassRecord {
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
    }
}

/// Runs `bypass` for every production record, in the records' order.
/// `iq` is the whole recording, sampled at `geo.sample_rate_hz` around the
/// uncalibrated `center_hz`; `geo` and `tol` are the production run's.
/// The RBN-fed bypasses only: the synthetic ones take their truth as
/// data ([`run_synthetic_bypass`]).
pub fn run_bypass(
    bypass: Bypass,
    iq: &[Complex32],
    center_hz: f64,
    cfg: &PipelineConfig,
    production: &[Record],
    geo: &Geometry,
    tol: &Tolerances,
) -> Result<(Vec<BypassRecord>, BypassSummary)> {
    let input = ChannelInput {
        iq,
        center_hz,
        factor: manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    if matches!(bypass, Bypass::KnownTransitions | Bypass::KnownTranscript) {
        anyhow::bail!(
            "bypass {} takes synthetic truth; call run_synthetic_bypass",
            bypass.label()
        );
    }
    let mut records = Vec::with_capacity(production.len());
    for prod in production {
        let mut line = blank_line(bypass, prod);
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
        // `known-timing` needs a truth speed and a seedable engine; both
        // unavailable reasons are the bypass's own honest report, not an
        // error.
        let seed = match bypass {
            Bypass::KnownFrequency => None,
            Bypass::KnownTiming => {
                if matches!(cfg.decode.engine, manta_decode::decoder::Engine::Hsmm) {
                    line.detail = Some("hsmmEngine".to_string());
                    records.push(line);
                    continue;
                }
                match op.wpm {
                    Some(w) => Some(w),
                    None => {
                        line.detail = Some("noRbnWpm".to_string());
                        records.push(line);
                        continue;
                    }
                }
            }
            Bypass::KnownTransitions | Bypass::KnownTranscript => {
                unreachable!("rejected before the loop");
            }
        };
        let ev = channel_evidence(&input, cfg, geo, &op, covered, seed)?;
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

/// Runs one synthetic bypass (`known-transitions` or `known-transcript`)
/// for the one opportunity `op` names, against the production record of
/// the same opportunity. Missing or ambiguous production matches are
/// errors. Keying truth unavailable to the production pipeline
/// crosses as plain data (`SyntheticTruth`, built by
/// `manta_testkit::funnel_truth`); the stages past the bypass's forced
/// ones are honestly evaluated through the production validator.
pub fn run_synthetic_bypass(
    bypass: Bypass,
    op: &Opportunity,
    truth: &SyntheticTruth,
    cfg: &PipelineConfig,
    production: &[Record],
    geo: &Geometry,
    tol: &Tolerances,
) -> Result<(BypassRecord, BypassSummary)> {
    if !matches!(bypass, Bypass::KnownTransitions | Bypass::KnownTranscript) {
        anyhow::bail!("bypass {} is RBN-fed; call run_bypass", bypass.label());
    }
    // These fields are copied unchanged from the opportunity by attribute.
    // Calls alone are not unique across episodes in a recording.
    let mut matches = production.iter().filter(|r| {
        r.call == op.call
            && r.frequency_hz == op.freq_hz
            && r.first_s == op.first_s
            && r.last_s == op.last_s
            && r.rows == op.rows
            && r.snr_db == op.snr_db
            && r.wpm == op.wpm
    });
    let Some(prod) = matches.next() else {
        anyhow::bail!("no production record for synthetic opportunity {}", op.call);
    };
    if matches.next().is_some() {
        anyhow::bail!(
            "ambiguous production records for synthetic opportunity {}",
            op.call
        );
    }
    let mut line = blank_line(bypass, prod);
    if bypass == Bypass::KnownTransitions
        && matches!(cfg.decode.engine, manta_decode::decoder::Engine::Hsmm)
    {
        // The transitions feed needs the speed seed; the hsmm engine's
        // speed never passes through it.
        line.detail = Some("hsmmEngine".to_string());
        let summary = summarize_bypass(bypass, &[line.clone()]);
        return Ok((line, summary));
    }
    let ev = match bypass {
        Bypass::KnownTransitions => transitions_evidence(truth, op, cfg, geo)?,
        Bypass::KnownTranscript => transcript_evidence(truth, op, cfg, geo)?,
        _ => unreachable!("checked above"),
    };
    let r = attribute(op, &ev, geo, tol);
    line.available = true;
    line.reached = r.reached;
    line.lost_at = r.lost_at;
    line.detail = r.detail;
    line.speed_wpm = r.speed_wpm;
    line.track_text = r.track_text;
    line.decisions = r.decisions;
    let summary = summarize_bypass(bypass, &[line.clone()]);
    Ok((line, summary))
}

/// The recording a channel decode runs over: the whole IQ, its
/// uncalibrated center, and the calibration factor.
struct ChannelInput<'a> {
    iq: &'a [Complex32],
    center_hz: f64,
    factor: f64,
}

/// Decodes the truth frequency's channel over `covered` with a fresh
/// decoder and validator, and returns that one track's evidence. With
/// `seed_wpm`, the decoder is seeded at that truth speed first (the
/// `known-timing` experiment) -- one more stage forced by construction.
fn channel_evidence(
    input: &ChannelInput<'_>,
    cfg: &PipelineConfig,
    geo: &Geometry,
    op: &Opportunity,
    covered: [f64; 2],
    seed_wpm: Option<f32>,
) -> Result<Evidence> {
    const TRACK_ID: u32 = 1;
    let iq = input.iq;
    let fs = geo.sample_rate_hz;
    let raw_freq_hz = op.freq_hz / input.factor;
    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, input.center_hz)
        .map_err(anyhow::Error::msg)?;
    let n = ch.n_channels();
    let delta = fs / n as f64;
    let k0 =
        (((raw_freq_hz - input.center_hz) / delta).round() as i64).rem_euclid(n as i64) as usize;
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
    if let Some(w) = seed_wpm {
        dec.seed_speed_wpm(w);
    }
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
    evidence_from_events(events, TRACK_ID, cfg, fs, op.freq_hz, covered)
}

/// `known-transitions`: the exact keying envelope, hop by hop, into a
/// fresh seeded decoder. Detection, admission and timing are forced by
/// construction; the text and every validator gate stay honest.
fn transitions_evidence(
    truth: &SyntheticTruth,
    op: &Opportunity,
    cfg: &PipelineConfig,
    geo: &Geometry,
) -> Result<Evidence> {
    const TRACK_ID: u32 = 1;
    let fs = geo.sample_rate_hz;
    // The validator applies `freq_correction_ppm` to decoder frequencies
    // itself, so events carry the uncalibrated value (as `channel_evidence`
    // does); attribution keeps the calibrated opportunity frequency.
    let raw_freq_hz = uncalibrated(op.freq_hz, cfg)?;
    let mut dec = TrackDecoder::new(TRACK_ID, cfg.decode.clone());
    dec.set_freq_hz(raw_freq_hz);
    dec.set_snr_2500_db(0.0);
    if !dec.seed_speed_wpm(truth.wpm) {
        anyhow::bail!("known-transitions needs a seedable engine");
    }
    let start_ts = truth.envelope.first().map(|&(ts, _)| ts).unwrap_or(0);
    let end_ts = truth.envelope.last().map(|&(ts, _)| ts).unwrap_or(0);
    let mut events = vec![DecoderEvent::TrackPromoted {
        track_id: TRACK_ID,
        sample_ts: start_ts,
        freq_hz: raw_freq_hz,
    }];
    for &(ts, a) in &truth.envelope {
        events.extend(dec.push_envelope(a, ts));
    }
    events.extend(dec.finish());
    events.push(DecoderEvent::TrackClosed {
        track_id: TRACK_ID,
        closure: ClosureKind::SignalEnded,
    });
    let span = [start_ts as f64 / fs, end_ts as f64 / fs];
    evidence_from_events(events, TRACK_ID, cfg, fs, op.freq_hz, span)
}

/// `known-transcript`: decoder events synthesized straight from the known
/// words, at the known speed. Everything up to and including the text
/// stages is forced; the validator's self-ID and evidence gates are
/// evaluated exactly as production would.
fn transcript_evidence(
    truth: &SyntheticTruth,
    op: &Opportunity,
    cfg: &PipelineConfig,
    geo: &Geometry,
) -> Result<Evidence> {
    const TRACK_ID: u32 = 1;
    let fs = geo.sample_rate_hz;
    let Some(&(start_ts, _)) = truth.words.iter().flat_map(|w| w.chars.first()).next() else {
        anyhow::bail!("known-transcript needs at least one known word");
    };
    // See `transitions_evidence`: events carry the uncalibrated frequency.
    let raw_freq_hz = uncalibrated(op.freq_hz, cfg)?;
    // Unlike the decoder-fed bypasses, this stream has no TrackDecoder
    // to emit metadata. The validator needs it before evaluating words.
    let mut events = vec![
        DecoderEvent::TrackPromoted {
            track_id: TRACK_ID,
            sample_ts: start_ts,
            freq_hz: raw_freq_hz,
        },
        DecoderEvent::TrackMeta {
            track_id: TRACK_ID,
            sample_ts: start_ts,
            freq_hz: raw_freq_hz,
            // A placeholder, as in the other bypasses; no measured SNR.
            snr_2500_db: 0.0,
            last_char_ts: start_ts,
        },
    ];
    // Each character at its real keyed end and each word boundary at the
    // word's real end: the silence between words is the truth's, so the
    // validator's turn and frame decisions see the actual timing.
    let mut end_ts = start_ts;
    for w in &truth.words {
        for (c, &(_, char_end)) in w.word.chars().zip(&w.chars) {
            events.push(DecoderEvent::char_decoded(
                TRACK_ID,
                char_end,
                Glyph::Char(c.to_ascii_uppercase()),
                1.0,
            ));
        }
        if let Some(&(_, word_end)) = w.chars.last() {
            events.push(DecoderEvent::word_boundary(TRACK_ID, word_end));
            end_ts = end_ts.max(word_end);
        }
    }
    events.push(DecoderEvent::SpeedUpdate {
        track_id: TRACK_ID,
        wpm: truth.wpm,
    });
    events.push(DecoderEvent::TrackClosed {
        track_id: TRACK_ID,
        closure: ClosureKind::SignalEnded,
    });
    let span = [start_ts as f64 / fs, end_ts as f64 / fs];
    evidence_from_events(events, TRACK_ID, cfg, fs, op.freq_hz, span)
}

/// `freq_hz` (calibrated) back in the receiver's uncalibrated frame.
fn uncalibrated(freq_hz: f64, cfg: &PipelineConfig) -> Result<f64> {
    let factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(freq_hz / factor)
}

/// The shared tail of every bypass runner: one track's decoder events
/// through the production validator and observer, folded into the
/// evidence `attribute` takes. The track is confirmed and leased by
/// construction (that is what the forced stages mean); `span` is its
/// `[born_s, end_s]`.
fn evidence_from_events(
    events: Vec<DecoderEvent>,
    track_id: u32,
    cfg: &PipelineConfig,
    fs: f64,
    spawn_freq_hz: f64,
    span: [f64; 2],
) -> Result<Evidence> {
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
    let t = tracks.remove(&track_id).unwrap_or_default();
    let track = TrackFacts {
        track_id,
        spawn_freq_hz,
        freq_hz: spawn_freq_hz,
        born_s: span[0],
        end_s: Some(span[1]),
        confirmed: true,
        leased: true,
        close: None,
        speeds: t.speeds.iter().map(|&(ts, w)| (secs(ts), w)).collect(),
        text: t.text.trim().to_string(),
        words: timed(t.words, secs),
        readings: timed(t.readings, secs),
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

    fn truth_words(
        text: &str,
        spec: &manta_testkit::keyer::KeyerSpec,
        fs: f64,
        t0_s: f64,
    ) -> Vec<TruthWord> {
        manta_testkit::funnel_truth::transcript_items(text, spec, fs, t0_s)
            .unwrap()
            .into_iter()
            .map(|i| TruthWord {
                word: i.word,
                chars: i.chars,
            })
            .collect()
    }

    #[test]
    fn known_transcript_delivers_repeated_self_id() {
        let fs = 96_000.0;
        let geo = Geometry {
            sample_rate_hz: fs,
            center_freq_hz: 14_000_000.0,
            duration_s: 45.0,
            passband_hz: (13_952_000.0, 14_048_000.0),
            channel_width_hz: 93.75,
            freq_correction_ppm: 0.0,
        };
        let op = Opportunity {
            call: "W1AW".into(),
            freq_hz: 14_012_300.0,
            first_s: 25.0,
            last_s: 25.0,
            rows: 1,
            snr_db: 20,
            wpm: Some(20.0),
        };
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        let truth = SyntheticTruth {
            wpm: 20.0,
            words: truth_words("CQ CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K", &spec, fs, 10.0),
            ..SyntheticTruth::default()
        };
        let tol = Tolerances::default();
        let production = attribute(&op, &Evidence::default(), &geo, &tol);
        let (line, summary) = run_synthetic_bypass(
            Bypass::KnownTranscript,
            &op,
            &truth,
            &PipelineConfig::default(),
            &[production],
            &geo,
            &tol,
        )
        .unwrap();
        assert_eq!(line.lost_at, None, "{line:?}");
        assert_eq!(summary.delivered, 1);
        let evidence = transcript_evidence(&truth, &op, &PipelineConfig::default(), &geo).unwrap();
        assert!(evidence
            .spots
            .iter()
            .any(|s| s.call == op.call && s.freq_hz == op.freq_hz));
    }

    fn geo_96k(ppm: f64) -> Geometry {
        Geometry {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 14_000_000.0,
            duration_s: 60.0,
            passband_hz: (13_952_000.0, 14_048_000.0),
            channel_width_hz: 93.75,
            freq_correction_ppm: ppm,
        }
    }

    fn op_at(call: &str, wpm: f32) -> Opportunity {
        Opportunity {
            call: call.into(),
            freq_hz: 14_012_300.0,
            first_s: 10.0,
            last_s: 10.0,
            rows: 1,
            snr_db: 20,
            wpm: Some(wpm),
        }
    }

    fn truth_at(text: &str, wpm: f32, fs: f64, t0_s: f64) -> SyntheticTruth {
        let spec = manta_testkit::keyer::KeyerSpec::new(wpm);
        let segs = manta_testkit::funnel_truth::keying_segments(text, &spec).unwrap();
        let total_ms: f64 = segs.iter().map(|s| s.1).sum();
        SyntheticTruth {
            wpm,
            envelope: manta_testkit::funnel_truth::envelope_at_hops(
                &segs,
                375.0,
                fs,
                t0_s,
                total_ms / 1000.0,
            ),
            words: truth_words(text, &spec, fs, t0_s),
        }
    }

    /// Codex P1 on #523: the transcript's events carry each word's real
    /// keyed duration. A fixed five-dit character step ended each
    /// synthetic word early (a 10 WPM OH2XYZ after 3.6 s instead of its
    /// keyed length), inventing silence before the next word that the
    /// validator's turn and frame decisions then read as real. Every
    /// character now lands at its keyed end, so each word spans exactly
    /// its truth.
    #[test]
    fn known_transcript_words_span_their_keyed_durations() {
        let geo = geo_96k(0.0);
        let fs = geo.sample_rate_hz;
        let op = op_at("OH2XYZ", 10.0);
        let truth = truth_at("CQ CQ DE OH2XYZ OH2XYZ K", 10.0, fs, 10.0);
        let evidence = transcript_evidence(&truth, &op, &PipelineConfig::default(), &geo).unwrap();
        let words = &evidence.tracks[0].words;
        assert_eq!(words.len(), truth.words.len());
        for ((_, end_s, word), want) in words.iter().zip(&truth.words) {
            assert_eq!(word, &want.word);
            let keyed_end_s = want.chars.last().unwrap().1 as f64 / fs;
            assert!(
                (end_s - keyed_end_s).abs() < 1e-6,
                "{word} ends at {end_s} s, keyed end {keyed_end_s} s"
            );
        }
    }

    /// Codex P1 on #523: the validator applies `freq_correction_ppm`
    /// itself, so synthetic decoder events carry the uncalibrated
    /// frequency; the delivered spot lands on the opportunity.
    #[test]
    fn synthetic_bypasses_do_not_calibrate_twice() {
        let geo = geo_96k(50.0);
        let op = op_at("W1AW", 20.0);
        let truth = truth_at(
            "CQ CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K",
            20.0,
            geo.sample_rate_hz,
            10.0,
        );
        let cfg = PipelineConfig {
            freq_correction_ppm: 50.0,
            ..PipelineConfig::default()
        };
        let evidence = transcript_evidence(&truth, &op, &cfg, &geo).unwrap();
        let spot = evidence
            .spots
            .iter()
            .find(|s| s.call == op.call)
            .expect("the transcript spots");
        assert!(
            (spot.freq_hz - op.freq_hz).abs() < 1.0,
            "spot at {} Hz, opportunity at {} Hz",
            spot.freq_hz,
            op.freq_hz
        );
        let tol = Tolerances::default();
        let production = attribute(&op, &Evidence::default(), &geo, &tol);
        let (line, _) = run_synthetic_bypass(
            Bypass::KnownTranscript,
            &op,
            &truth,
            &cfg,
            &[production],
            &geo,
            &tol,
        )
        .unwrap();
        assert_eq!(line.lost_at, None, "{line:?}");
    }

    /// MAN-493: the four experiments' labels and forced stages.
    #[test]
    fn bypass_labels_and_forced_stages() {
        assert_eq!(Bypass::KnownFrequency.label(), "knownFrequency");
        assert_eq!(
            Bypass::KnownFrequency.forced_stages(),
            &[Stage::DetectableEnergy, Stage::AdmittedTrack]
        );
        for (b, label) in [
            (Bypass::KnownTiming, "knownTiming"),
            (Bypass::KnownTransitions, "knownTransitions"),
            (Bypass::KnownTranscript, "knownTranscript"),
        ] {
            assert_eq!(b.label(), label);
            assert_eq!(
                b.forced_stages(),
                &[
                    Stage::DetectableEnergy,
                    Stage::AdmittedTrack,
                    Stage::UsableTiming
                ]
            );
        }
        // Every variant is distinct.
        assert_eq!(
            [
                Bypass::KnownFrequency,
                Bypass::KnownTiming,
                Bypass::KnownTransitions,
                Bypass::KnownTranscript
            ]
            .map(Bypass::label),
            [
                "knownFrequency",
                "knownTiming",
                "knownTransitions",
                "knownTranscript"
            ]
        );
    }

    /// MAN-493: the synthetic runner refuses RBN-fed bypasses, and vice
    /// versa, instead of silently running the wrong experiment.
    #[test]
    fn synthetic_and_rbn_runners_reject_the_wrong_bypass() {
        let geo = Geometry {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 14_000_000.0,
            duration_s: 45.0,
            passband_hz: (13_952_000.0, 14_048_000.0),
            channel_width_hz: 93.75,
            freq_correction_ppm: 0.0,
        };
        let op = Opportunity {
            call: "W1AW".into(),
            freq_hz: 14_012_300.0,
            first_s: 25.0,
            last_s: 25.0,
            rows: 1,
            snr_db: 20,
            wpm: Some(20.0),
        };
        let cfg = PipelineConfig::default();
        assert!(run_synthetic_bypass(
            Bypass::KnownFrequency,
            &op,
            &SyntheticTruth::default(),
            &cfg,
            &[],
            &geo,
            &Tolerances::default()
        )
        .is_err());
        assert!(run_bypass(
            Bypass::KnownTransitions,
            &[],
            14_000_000.0,
            &cfg,
            &[],
            &geo,
            &Tolerances::default()
        )
        .is_err());
    }

    #[test]
    fn synthetic_bypass_matches_exact_production_episode() {
        let geo = Geometry {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 14_000_000.0,
            duration_s: 2100.0,
            passband_hz: (13_952_000.0, 14_048_000.0),
            channel_width_hz: 93.75,
            freq_correction_ppm: 0.0,
        };
        let early = Opportunity {
            call: "W1AW".into(),
            freq_hz: 14_012_300.0,
            first_s: 25.0,
            last_s: 25.0,
            rows: 1,
            snr_db: 20,
            wpm: Some(20.0),
        };
        let mut later = early.clone();
        later.freq_hz += 10_000.0;
        later.first_s = 2025.0;
        later.last_s = 2026.0;
        let tol = Tolerances::default();
        let mut early_record = attribute(&early, &Evidence::default(), &geo, &tol);
        early_record.lost_at = None;
        let later_record = attribute(&later, &Evidence::default(), &geo, &tol);
        let mut cfg = PipelineConfig::default();
        cfg.decode.engine = manta_decode::decoder::Engine::Hsmm;
        let truth = SyntheticTruth {
            wpm: 20.0,
            words: truth_words(
                "CQ",
                &manta_testkit::keyer::KeyerSpec::new(20.0),
                geo.sample_rate_hz,
                2025.0,
            ),
            ..SyntheticTruth::default()
        };
        let run = |bypass, records: &[Record]| {
            run_synthetic_bypass(bypass, &later, &truth, &cfg, records, &geo, &tol)
        };
        for bypass in [Bypass::KnownTransitions, Bypass::KnownTranscript] {
            let (line, summary) =
                run(bypass, &[early_record.clone(), later_record.clone()]).unwrap();
            assert_eq!(line.first_s, later.first_s);
            assert_eq!(line.frequency_hz, later.freq_hz);
            assert_eq!(line.production_lost_at, later_record.lost_at);
            assert_eq!(summary.upper_bound, 0);
            assert!(run(bypass, std::slice::from_ref(&early_record)).is_err());
            assert!(run(bypass, &[later_record.clone(), later_record.clone()]).is_err());
            // First timestamp alone is not the full episode identity.
            let mut wrong_end = later_record.clone();
            wrong_end.last_s += 1.0;
            assert!(run(bypass, &[wrong_end]).is_err());
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
