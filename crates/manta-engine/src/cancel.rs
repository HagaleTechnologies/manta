//! MAN-204: guarded two-stage cancellation, an offline experiment. Decode
//! the buffer as `decode_samples` does, estimate each strong CW station's
//! received waveform from the raw IQ (`manta_dsp::cancel`), subtract it from
//! a separate working buffer only where the second-carrier evidence and the
//! residual check both hold, and decode that residual with its own
//! validator. Offline experiment; not part of any public output: no spot,
//! telnet line, JSON field, metric or uplink message reads this module, and
//! `CancellationReport::original` is `decode_samples`' result unchanged.
//! See docs/DECISIONS/2026-11-03-man204-guarded-two-stage-cancellation.md.

use crate::{decode_samples, DecodeReport, NoSignal, PipelineConfig};
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_dsp::cancel::{
    classify, estimate_strong, residual_check, second_carrier_evidence, subtract, Hypothesis,
    ResidualCheck, SecondCarrierEvidence, MAX_CARRIER_STEP_HZ, MIN_ELEMENTS,
};
use manta_spot::{Spot, SpotMode};
use num_complex::Complex32;

/// D7: a residual spot further than this from a subtracted candidate's
/// evidence frequency is not admitted (V7's frequency gate).
pub const ADMIT_RADIUS_HZ: f64 = 15.0;

/// Offline experiment; not part of any public output. Knobs of
/// [`decode_with_cancellation`].
#[derive(Debug, Clone, PartialEq)]
pub struct CancelOptions {
    /// D9: at most this many strong stations are examined.
    pub max_candidates: usize,
    /// D5: subtract only `TwoSignalCw` candidates. Off, every candidate
    /// whose estimate is stable and whose residual check is clean is
    /// subtracted (measurement only).
    pub evidence_gate: bool,
    /// Test-and-measurement override of every candidate's carrier hint
    /// (offset from the centre, Hz); `None` takes it from the spot.
    pub carrier_hint_override_hz: Option<f64>,
}

impl Default for CancelOptions {
    fn default() -> Self {
        CancelOptions {
            max_candidates: 8,
            evidence_gate: true,
            carrier_hint_override_hz: None,
        }
    }
}

/// Offline experiment; not part of any public output.
#[derive(Debug)]
pub struct CancellationReport {
    /// `decode_samples`' result, unchanged: the only public branch.
    pub original: DecodeReport,
    /// One per examined strong station, in D9 order.
    pub candidates: Vec<CandidateReport>,
    /// `None`: nothing was subtracted. `Some` with no events: the residual
    /// held no decodable signal.
    pub residual: Option<ResidualBranch>,
    pub recovered: Vec<RecoveredIdentity>,
    pub rejected: Vec<RejectedIdentity>,
}

/// Offline experiment; not part of any public output. One strong station.
#[derive(Debug, Clone)]
pub struct CandidateReport {
    pub track_id: u32,
    pub callsign: String,
    pub snr_db: f32,
    /// Offset from the centre the estimate started from.
    pub carrier_hint_hz: f64,
    /// Refined offset from the centre.
    pub carrier_hz: f64,
    pub fits: usize,
    pub evidence: Option<SecondCarrierEvidence>,
    pub hypothesis: Option<Hypothesis>,
    pub check: Option<ResidualCheck>,
    pub outcome: CandidateOutcome,
}

impl CandidateReport {
    /// Absolute frequency (uncalibrated) of the neighbour the evidence
    /// points at: `carrier + Δf`, when the evidence holds an offset.
    pub fn evidence_freq_hz(&self, center_freq_hz: f64) -> Option<f64> {
        let offset = match self.hypothesis {
            Some(Hypothesis::TwoSignalCw { offset_hz }) => Some(offset_hz),
            _ => self.evidence.and_then(|e| e.offset_hz),
        }?;
        Some(center_freq_hz + self.carrier_hz + offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOutcome {
    Subtracted,
    Skipped(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    TooFewElements,
    UnstableCarrier,
    Hypothesis,
    DirtyResidual,
}

/// Offline experiment; not part of any public output. The residual decode.
#[derive(Debug, Clone, Default)]
pub struct ResidualBranch {
    pub events: Vec<DecoderEvent>,
    pub spots: Vec<Spot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredIdentity {
    pub callsign: String,
    pub freq_hz: f64,
    pub snr_db: f32,
    pub track_id: u32,
    pub beside_track_id: u32,
    pub beside_callsign: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RejectedIdentity {
    pub callsign: String,
    pub freq_hz: f64,
    pub reason: RejectReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    OutsideNeighbourhood,
    DuplicateOfStrong,
    DuplicateOfOriginal,
}

/// A strong station chosen from the original branch's spots.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    track_id: u32,
    callsign: String,
    freq_hz: f64,
    snr_db: f32,
}

/// D9: the CW spots grouped by track (first spot's call and frequency, the
/// track's best SNR), ordered by SNR descending then track id ascending,
/// capped at `max`.
fn select_candidates(spots: &[Spot], max: usize) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for s in spots.iter().filter(|s| s.mode == SpotMode::Cw) {
        match out.iter_mut().find(|c| c.track_id == s.track_id) {
            Some(c) => c.snr_db = c.snr_db.max(s.snr_db),
            None => out.push(Candidate {
                track_id: s.track_id,
                callsign: s.callsign.clone(),
                freq_hz: s.freq_hz,
                snr_db: s.snr_db,
            }),
        }
    }
    out.sort_by(|a, b| {
        b.snr_db
            .total_cmp(&a.snr_db)
            .then(a.track_id.cmp(&b.track_id))
    });
    out.truncate(max);
    out
}

/// D3: a spot's calibrated absolute frequency back to an uncalibrated
/// offset from the centre.
fn carrier_hint(spot_freq_hz: f64, calibration_factor: f64, center_freq_hz: f64) -> f64 {
    spot_freq_hz / calibration_factor - center_freq_hz
}

/// D7: admit each residual spot (in order) that lies within
/// `ADMIT_RADIUS_HZ` of a subtracted candidate's evidence frequency, is not
/// that candidate's call and is not among the original spots' calls.
/// `candidates` pairs each report with its evidence frequency (calibrated).
fn admit(
    residual: &[Spot],
    original: &[Spot],
    candidates: &[(&CandidateReport, Option<f64>)],
) -> (Vec<RecoveredIdentity>, Vec<RejectedIdentity>) {
    let mut recovered = Vec::new();
    let mut rejected = Vec::new();
    for s in residual {
        let near = candidates.iter().find(|(c, f)| {
            c.outcome == CandidateOutcome::Subtracted
                && f.is_some_and(|f| (s.freq_hz - f).abs() <= ADMIT_RADIUS_HZ)
        });
        let reject = |reason| RejectedIdentity {
            callsign: s.callsign.clone(),
            freq_hz: s.freq_hz,
            reason,
        };
        match near {
            None => rejected.push(reject(RejectReason::OutsideNeighbourhood)),
            Some((c, _)) if c.callsign == s.callsign => {
                rejected.push(reject(RejectReason::DuplicateOfStrong))
            }
            Some(_) if original.iter().any(|o| o.callsign == s.callsign) => {
                rejected.push(reject(RejectReason::DuplicateOfOriginal))
            }
            Some((c, _)) => recovered.push(RecoveredIdentity {
                callsign: s.callsign.clone(),
                freq_hz: s.freq_hz,
                snr_db: s.snr_db,
                track_id: s.track_id,
                beside_track_id: c.track_id,
                beside_callsign: c.callsign.clone(),
            }),
        }
    }
    (recovered, rejected)
}

/// Offline experiment; not part of any public output. Decode `iq` as
/// `decode_samples` does, then run guarded two-stage cancellation around
/// its strong stations and decode the residual separately (D1-D11).
/// Errors are `decode_samples`' own; an empty residual is `Some` empty.
pub fn decode_with_cancellation(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
    opts: &CancelOptions,
) -> Result<CancellationReport> {
    let original = decode_samples(iq, fs, center_freq_hz, cfg)?;
    let factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)?;
    let mut candidates = Vec::new();
    let mut residual: Option<Vec<Complex32>> = None;
    for c in select_candidates(&original.spots, opts.max_candidates) {
        let hint = opts
            .carrier_hint_override_hz
            .unwrap_or_else(|| carrier_hint(c.freq_hz, factor, center_freq_hz));
        let est = estimate_strong(iq, fs, hint);
        let mut report = CandidateReport {
            track_id: c.track_id,
            callsign: c.callsign,
            snr_db: c.snr_db,
            carrier_hint_hz: hint,
            carrier_hz: est.carrier_hz,
            fits: est.fits.len(),
            evidence: None,
            hypothesis: None,
            check: None,
            outcome: CandidateOutcome::Subtracted,
        };
        let skip = if est.fits.len() < MIN_ELEMENTS {
            Some(SkipReason::TooFewElements)
        } else if est.carrier_step_hz.abs() > MAX_CARRIER_STEP_HZ {
            Some(SkipReason::UnstableCarrier)
        } else {
            let evidence = second_carrier_evidence(iq, fs, &est);
            let hypothesis = classify(&evidence);
            report.evidence = Some(evidence);
            report.hypothesis = Some(hypothesis);
            if opts.evidence_gate && !matches!(hypothesis, Hypothesis::TwoSignalCw { .. }) {
                Some(SkipReason::Hypothesis)
            } else {
                let check = residual_check(iq, fs, &est);
                report.check = Some(check);
                (!check.clean()).then_some(SkipReason::DirtyResidual)
            }
        };
        match skip {
            Some(reason) => report.outcome = CandidateOutcome::Skipped(reason),
            None => subtract(residual.get_or_insert_with(|| iq.to_vec()), fs, &est),
        }
        candidates.push(report);
    }
    let residual = match residual {
        None => None,
        Some(buf) => Some(match decode_samples(&buf, fs, center_freq_hz, cfg) {
            Ok(r) => ResidualBranch {
                events: r.events,
                spots: r.spots,
            },
            Err(e) if e.downcast_ref::<NoSignal>().is_some() => ResidualBranch::default(),
            Err(e) => return Err(e),
        }),
    };
    let (recovered, rejected) = match &residual {
        None => (Vec::new(), Vec::new()),
        Some(branch) => {
            let near: Vec<_> = candidates
                .iter()
                .map(|c| (c, c.evidence_freq_hz(center_freq_hz).map(|f| f * factor)))
                .collect();
            admit(&branch.spots, &original.spots, &near)
        }
    };
    Ok(CancellationReport {
        original,
        candidates,
        residual,
        recovered,
        rejected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::{EmissionState, SpotType};

    fn spot(call: &str, freq_hz: f64, snr_db: f32, track_id: u32) -> Spot {
        Spot {
            callsign: call.into(),
            freq_hz,
            snr_db,
            wpm: 25.0,
            spot_type: SpotType::Cq,
            confidence: 1.0,
            emission_state: EmissionState::Confirmed,
            track_id,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
            mode: SpotMode::Cw,
            baud: None,
        }
    }

    fn candidate(call: &str, outcome: CandidateOutcome) -> CandidateReport {
        CandidateReport {
            track_id: 7,
            callsign: call.into(),
            snr_db: 30.0,
            carrier_hint_hz: 10_031.25,
            carrier_hz: 10_031.25,
            fits: 40,
            evidence: None,
            hypothesis: Some(Hypothesis::TwoSignalCw { offset_hz: 40.0 }),
            check: None,
            outcome,
        }
    }

    const CENTER: f64 = 14_030_000.0;
    const NEIGH: f64 = CENTER + 10_031.25 + 40.0;

    #[test]
    fn admit_recovers_a_new_call_beside_a_subtracted_candidate() {
        let c = candidate("W1AW", CandidateOutcome::Subtracted);
        let near = [(&c, c.evidence_freq_hz(CENTER))];
        let original = [spot("W1AW", CENTER + 10_031.25, 30.0, 7)];
        let (rec, rej) = admit(&[spot("K5ARH", NEIGH + 3.0, 10.0, 1)], &original, &near);
        assert!(rej.is_empty());
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].callsign, "K5ARH");
        assert_eq!(rec[0].beside_callsign, "W1AW");
        assert_eq!(rec[0].beside_track_id, 7);
    }

    #[test]
    fn admit_rejects_each_reason() {
        let c = candidate("W1AW", CandidateOutcome::Subtracted);
        let near = [(&c, c.evidence_freq_hz(CENTER))];
        let original = [
            spot("W1AW", CENTER + 10_031.25, 30.0, 7),
            spot("N0XX", CENTER + 2_000.0, 20.0, 9),
        ];
        let residual = [
            spot("W1AW", NEIGH, 10.0, 1),
            spot("N0XX", NEIGH - 5.0, 10.0, 2),
            spot("K5ARH", NEIGH + 15.1, 10.0, 3),
        ];
        let (rec, rej) = admit(&residual, &original, &near);
        assert!(rec.is_empty());
        let reasons: Vec<_> = rej.iter().map(|r| r.reason).collect();
        assert_eq!(
            reasons,
            [
                RejectReason::DuplicateOfStrong,
                RejectReason::DuplicateOfOriginal,
                RejectReason::OutsideNeighbourhood
            ]
        );
    }

    #[test]
    fn admit_rejects_a_call_near_a_candidate_that_was_not_subtracted() {
        let c = candidate("W1AW", CandidateOutcome::Skipped(SkipReason::DirtyResidual));
        let near = [(&c, c.evidence_freq_hz(CENTER))];
        let (rec, rej) = admit(&[spot("K5ARH", NEIGH, 10.0, 1)], &[], &near);
        assert!(rec.is_empty());
        assert_eq!(rej[0].reason, RejectReason::OutsideNeighbourhood);
    }

    #[test]
    fn admit_without_an_evidence_offset_admits_nothing() {
        let mut c = candidate("W1AW", CandidateOutcome::Subtracted);
        c.hypothesis = Some(Hypothesis::OneSignal);
        let near = [(&c, c.evidence_freq_hz(CENTER))];
        let (rec, rej) = admit(&[spot("K5ARH", NEIGH, 10.0, 1)], &[], &near);
        assert!(rec.is_empty());
        assert_eq!(rej[0].reason, RejectReason::OutsideNeighbourhood);
    }

    #[test]
    fn candidates_are_ordered_by_snr_then_track_id_and_capped() {
        let snrs = [10.0, 30.0, 20.0, 30.0, 5.0, 25.0, 25.0, 15.0, 1.0, 12.0];
        let mut spots: Vec<Spot> = snrs
            .iter()
            .enumerate()
            .map(|(i, &s)| spot(&format!("K{i}AA"), 1.0e7 + i as f64, s, 10 - i as u32))
            .collect();
        // A second, weaker spot on the strongest track keeps that track's best SNR.
        spots.push(spot("K1AA", 1.0e7, 2.0, 9));
        let mut rtty = spot("RY1RY", 1.0e7, 99.0, 99);
        rtty.mode = SpotMode::Rtty;
        spots.push(rtty);
        let got: Vec<(u32, f32)> = select_candidates(&spots, 8)
            .iter()
            .map(|c| (c.track_id, c.snr_db))
            .collect();
        assert_eq!(
            got,
            [
                (7, 30.0),
                (9, 30.0),
                (4, 25.0),
                (5, 25.0),
                (8, 20.0),
                (3, 15.0),
                (1, 12.0),
                (10, 10.0)
            ]
        );
    }

    #[test]
    fn a_calibrated_spot_frequency_is_uncalibrated_for_the_hint() {
        let factor = manta_spot::calibration_factor_from_ppm(10.0).unwrap();
        let spot_hz = (CENTER + 10_031.25) * factor;
        let hint = carrier_hint(spot_hz, factor, CENTER);
        assert!((hint - 10_031.25).abs() < 1e-6, "{hint}");
        assert_eq!(hint, spot_hz / factor - CENTER);
    }
}
