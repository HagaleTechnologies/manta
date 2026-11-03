//! MAN-207: `manta decode --observations PATH` writes the validator's
//! observation record as JSON Lines, schema `manta-observations.v1`
//! (docs/OBSERVATIONS.md). The private `manta_spot::observation` types are
//! mapped here, with exhaustive destructuring, so a new field fails to
//! compile until it is mapped or deliberately left out. Local and opt-in:
//! not a public contract.

use anyhow::{Context, Result};
use manta_engine::ObservedRun;
use manta_server::live_state::{decision_label, reason_label};
use manta_spot::decision::{Decision, Rival};
use manta_spot::observation::{
    Cause, Detail, Draft, EvidenceLink, FreqSpan, LossCounts, Observation, SampleSpan, Scores,
    Source, TextTrace,
};
use serde::Serialize;
use std::io::Write;

pub const SCHEMA: &str = "manta-observations.v1";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    schema: &'static str,
    detail: &'static str,
    decoder_version: &'static str,
    sample_rate_hz: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceLine {
    observation: String,
    sample_ts: u64,
    message_distinct: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceLine {
    text: String,
    confidences: Vec<f32>,
    erasures: Vec<u16>,
    /// `[]` when no position had any; else one `[glyph, mass]` list per
    /// character.
    alternatives: Vec<Vec<(manta_decode::tree::Glyph, f32)>>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservationLine {
    kind: &'static str,
    seq: u64,
    observation: String,
    supersedes: Option<u64>,
    cause: &'static str,
    recorded_at: u64,
    input: u32,
    epoch: u32,
    track_id: u32,
    word_seq: u64,
    start_sample_ts: Option<u64>,
    end_sample_ts: Option<u64>,
    decided_sample_ts: u64,
    frequency: f64,
    frequency_span: Option<[f64; 2]>,
    call: String,
    spot_type: manta_spot::SpotType,
    decision: &'static str,
    reason: Option<&'static str>,
    reps: Option<u32>,
    required_reps: u32,
    pattern_tier: Option<&'static str>,
    word_confidence: Option<f32>,
    call_confidence: Option<f32>,
    rival: Option<String>,
    evidence: Vec<EvidenceLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<TraceLine>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionCount {
    decision: &'static str,
    reason: Option<&'static str>,
    count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryLine {
    kind: &'static str,
    records: usize,
    decisions: Vec<DecisionCount>,
    expired: u64,
    evicted: u64,
    orphaned_revisions: u64,
    unsent: u64,
}

pub fn detail_label(d: Detail) -> &'static str {
    match d {
        Detail::Summary => "summary",
        Detail::Records => "records",
        Detail::Text => "text",
    }
}

pub fn cause_label(c: Cause) -> &'static str {
    match c {
        Cause::Decided => "decided",
        Cause::Reclassified => "reclassified",
        Cause::Settled => "settled",
        Cause::Refined => "refined",
    }
}

fn observation_line(o: &Observation) -> ObservationLine {
    let Observation {
        seq,
        supersedes,
        recorded_at,
        draft,
    } = o;
    let Draft {
        id,
        cause,
        source: Source { input, epoch },
        span,
        freq,
        decision,
        scores: Scores {
            word_confidence,
            call_confidence,
        },
        evidence,
        trace,
    } = draft;
    let Decision {
        track_id: _,
        sample_ts,
        call,
        freq_hz,
        spot_type,
        verdict,
        reason,
        reps,
        required_reps,
        pattern_tier,
        rival,
        spot: _,
    } = decision;
    ObservationLine {
        kind: "observation",
        seq: *seq,
        observation: id.to_string(),
        supersedes: *supersedes,
        cause: cause_label(*cause),
        recorded_at: *recorded_at,
        input: *input,
        epoch: *epoch,
        track_id: id.track_id,
        word_seq: id.word_seq,
        start_sample_ts: span.map(|SampleSpan { start_ts, .. }| start_ts),
        end_sample_ts: span.map(|SampleSpan { end_ts, .. }| end_ts),
        decided_sample_ts: *sample_ts,
        frequency: *freq_hz,
        frequency_span: freq.map(|FreqSpan { lo_hz, hi_hz }| [lo_hz, hi_hz]),
        call: call.clone(),
        spot_type: *spot_type,
        decision: decision_label(*verdict),
        reason: reason.map(reason_label),
        reps: *reps,
        required_reps: *required_reps,
        pattern_tier: pattern_tier.map(|t| t.as_str()),
        word_confidence: *word_confidence,
        call_confidence: *call_confidence,
        rival: rival.as_ref().map(|Rival { call, .. }| call.clone()),
        evidence: evidence
            .iter()
            .map(
                |&EvidenceLink {
                     observation,
                     sample_ts,
                     message_distinct,
                 }| EvidenceLine {
                    observation: observation.to_string(),
                    sample_ts,
                    message_distinct,
                },
            )
            .collect(),
        trace: trace.as_ref().map(
            |TextTrace {
                 text,
                 confidences,
                 erasures,
                 alternatives,
                 truncated,
             }| TraceLine {
                text: text.clone(),
                confidences: confidences.clone(),
                erasures: erasures.clone(),
                alternatives: alternatives.clone(),
                truncated: *truncated,
            },
        ),
    }
}

fn summary_line(run: &ObservedRun) -> SummaryLine {
    let LossCounts {
        expired,
        evicted,
        orphaned_revisions,
        unsent,
    } = run.losses;
    SummaryLine {
        kind: "summary",
        records: run.records.len(),
        decisions: run
            .counts
            .iter()
            .filter(|&(_, _, n)| n > 0)
            .map(|(verdict, reason, count)| DecisionCount {
                decision: decision_label(verdict),
                reason: reason.map(reason_label),
                count,
            })
            .collect(),
        expired,
        evicted,
        orphaned_revisions,
        unsent,
    }
}

/// Writes the header, one line per record in `seq` order, and the summary
/// trailer. Returns the number of records written.
pub fn write(
    out: impl Write,
    detail: Detail,
    sample_rate_hz: f64,
    run: &ObservedRun,
) -> Result<usize> {
    let mut out = std::io::BufWriter::new(out);
    let header = Header {
        schema: SCHEMA,
        detail: detail_label(detail),
        decoder_version: crate::build_info::DECODER_VERSION,
        sample_rate_hz,
    };
    writeln!(out, "{}", serde_json::to_string(&header)?)?;
    for o in &run.records {
        writeln!(out, "{}", serde_json::to_string(&observation_line(o))?)?;
    }
    writeln!(out, "{}", serde_json::to_string(&summary_line(run))?)?;
    out.flush().context("write the observation file")?;
    Ok(run.records.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_decode::tree::Glyph;
    use manta_spot::observation::{ObservationCounts, ObservationId};
    use manta_spot::{Reason, Verdict};

    fn documented() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/OBSERVATIONS.md"
        ))
        .unwrap()
    }

    fn full_run() -> ObservedRun {
        let mut log =
            manta_spot::ObservationLog::new(1000.0, manta_spot::ObservationPolicy::text());
        let id = ObservationId {
            track_id: 1,
            word_seq: 2,
        };
        let decision = Decision {
            track_id: 1,
            sample_ts: 577_792,
            call: "K5ARH".into(),
            freq_hz: 14_012_340.2,
            spot_type: manta_spot::SpotType::De,
            verdict: Verdict::Withheld,
            reason: Some(Reason::Variant),
            reps: Some(1),
            required_reps: 2,
            pattern_tier: Some(manta_spot::PatternTier::Common),
            rival: Some(Rival {
                call: "K5ARM".into(),
                support: manta_spot::Support {
                    reps: 2,
                    conf_sum: 1.8,
                },
            }),
            spot: None,
        };
        let mut draft = Draft::new(id, Cause::Decided, Source::default(), decision);
        draft.span = Some(SampleSpan {
            start_ts: 540_000,
            end_ts: 577_792,
        });
        draft.freq = Some(FreqSpan {
            lo_hz: 14_012_339.6,
            hi_hz: 14_012_340.9,
        });
        draft.scores.word_confidence = Some(0.97);
        draft.scores.call_confidence = Some(0.49);
        draft.evidence = vec![EvidenceLink {
            observation: id,
            sample_ts: 577_792,
            message_distinct: true,
        }];
        draft.trace = Some(TextTrace::new(
            "K5ARH",
            &[0.9; 5],
            &[
                vec![],
                vec![],
                vec![],
                vec![],
                vec![(Glyph::Char('M'), 0.3)],
            ],
        ));
        log.append(draft, 577_792);
        log.count_decision(Some(Reason::Variant));
        ObservedRun {
            records: log.take_appended(),
            counts: *log.counts(),
            losses: log.losses(),
        }
    }

    fn keys(line: &str) -> Vec<String> {
        fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(m) => {
                    for (k, v) in m {
                        out.push(k.clone());
                        walk(v, out);
                    }
                }
                serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&serde_json::from_str(line).unwrap(), &mut out);
        out
    }

    #[test]
    fn every_observation_field_is_documented() {
        let mut buf = Vec::new();
        write(&mut buf, Detail::Text, 96_000.0, &full_run()).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header, one observation, summary");
        assert!(lines[1].contains(r#""trace":"#));
        assert!(lines[1].contains(r#""rival":"K5ARM""#));
        let doc = documented();
        for line in lines {
            for key in keys(line) {
                assert!(
                    doc.contains(&format!("`{key}`")),
                    "docs/OBSERVATIONS.md must document `{key}`"
                );
            }
        }
    }

    #[test]
    fn every_cause_and_detail_label_is_documented() {
        let doc = documented();
        for c in [
            Cause::Decided,
            Cause::Reclassified,
            Cause::Settled,
            Cause::Refined,
        ] {
            assert!(doc.contains(&format!("`{}`", cause_label(c))), "{c:?}");
        }
        for d in [Detail::Records, Detail::Text] {
            assert!(doc.contains(&format!("`{}`", detail_label(d))), "{d:?}");
        }
    }

    #[test]
    fn reason_labels_are_the_live_state_labels() {
        let mut counts_log =
            manta_spot::ObservationLog::new(1000.0, manta_spot::ObservationPolicy::summary());
        for r in Reason::ALL {
            counts_log.count_decision(Some(r));
        }
        let run = ObservedRun {
            records: Vec::new(),
            counts: *counts_log.counts(),
            losses: LossCounts::default(),
        };
        let summary = summary_line(&run);
        assert_eq!(summary.decisions.len(), Reason::ALL.len());
        for (line, r) in summary.decisions.iter().zip(Reason::ALL) {
            assert_eq!(line.reason, Some(reason_label(r)));
            assert_eq!(line.decision, decision_label(r.verdict()));
        }
        assert_eq!(
            ObservationCounts::default().decided(Verdict::Spotted, None),
            0
        );
    }
}
