//! MAN-198: the loss funnel's pure attribution -- opportunities, the
//! evidence one production pass left, and the one earliest failed stage
//! each opportunity is charged to. No I/O and no decoding.

use super::trace::{SpawnVeto, TrackCloseReason};
use manta_spot::Reason;
use serde::Serialize;
use std::collections::BTreeMap;

/// The nine pipeline stages, in order. An opportunity's `lost_at` is the
/// first one it did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    InputCoverage,
    DetectableEnergy,
    AdmittedTrack,
    UsableTiming,
    TextContainsCall,
    SelfIdRecognized,
    SufficientEvidence,
    Emitted,
    Delivered,
}

impl Stage {
    /// Every stage, in funnel order.
    pub const ALL: [Stage; 9] = [
        Stage::InputCoverage,
        Stage::DetectableEnergy,
        Stage::AdmittedTrack,
        Stage::UsableTiming,
        Stage::TextContainsCall,
        Stage::SelfIdRecognized,
        Stage::SufficientEvidence,
        Stage::Emitted,
        Stage::Delivered,
    ];

    /// The report's camelCase key.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::InputCoverage => "inputCoverage",
            Stage::DetectableEnergy => "detectableEnergy",
            Stage::AdmittedTrack => "admittedTrack",
            Stage::UsableTiming => "usableTiming",
            Stage::TextContainsCall => "textContainsCall",
            Stage::SelfIdRecognized => "selfIdRecognized",
            Stage::SufficientEvidence => "sufficientEvidence",
            Stage::Emitted => "emitted",
            Stage::Delivered => "delivered",
        }
    }

    fn next(self) -> Option<Stage> {
        Stage::ALL.get(self as usize + 1).copied()
    }
}

/// Where the truth opportunities came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruthLayer {
    /// One RBN spotter's rows (`manta oracle --funnel`).
    RbnSingleSpotter,
    /// A synthetic scene's own keying truth.
    SyntheticExact,
}

impl TruthLayer {
    /// The header's `truthLayer` value.
    pub fn label(self) -> &'static str {
        match self {
            TruthLayer::RbnSingleSpotter => "rbn-single-spotter",
            TruthLayer::SyntheticExact => "synthetic-exact",
        }
    }
}

/// One truth opportunity: a call on one frequency over one span of time.
#[derive(Debug, Clone, PartialEq)]
pub struct Opportunity {
    pub call: String,
    /// Absolute RF Hz.
    pub freq_hz: f64,
    /// Seconds from the recording start of the first and last truth report.
    pub first_s: f64,
    pub last_s: f64,
    /// Truth reports in the opportunity.
    pub rows: usize,
    /// The strongest report's SNR, dB (the truth source's own reference).
    pub snr_db: i32,
    /// The truth speed, when the source gave one.
    pub wpm: Option<f32>,
}

/// The matching tolerances. Defaults: `manta grade`'s 500 Hz / 90 s for
/// call-keyed evidence, the bench's 150 Hz (plus half a channel) for
/// call-agnostic track evidence, the oracle's 40 s acoustic window, and a
/// 900 s episode gap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tolerances {
    pub track_tol_hz: f64,
    pub spot_tol_hz: f64,
    pub window_s: f64,
    pub spot_window_s: f64,
    pub episode_gap_s: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            track_tol_hz: 150.0,
            spot_tol_hz: 500.0,
            window_s: 40.0,
            spot_window_s: 90.0,
            episode_gap_s: 900.0,
        }
    }
}

/// The recording and front end the opportunities are judged against.
/// Calibrated Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub passband_hz: (f64, f64),
    pub channel_width_hz: f64,
    pub duration_s: f64,
    pub freq_correction_ppm: f64,
}

/// One track the production pass opened, as the funnel sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackFacts {
    pub track_id: u32,
    /// Birth channel centre, calibrated.
    pub spawn_freq_hz: f64,
    /// Latest known frequency (`TrackMeta`, else `TrackPromoted`, else the
    /// birth channel), calibrated.
    pub freq_hz: f64,
    pub born_s: f64,
    /// `None` while still open at the end of the recording.
    pub end_s: Option<f64>,
    pub confirmed: bool,
    /// Leased a decoder (`TrackPromoted`).
    pub leased: bool,
    pub close: Option<TrackCloseReason>,
    /// (seconds, WPM), each stamped with the track's latest event time.
    pub speeds: Vec<(f64, f32)>,
    /// One space per word boundary.
    pub text: String,
    /// (first character s, last character s, word): each decoded word,
    /// in decode order.
    pub words: Vec<(f64, f64, String)>,
    /// (first holding s, last holding s, word): words the decoder held as
    /// acoustically viable readings, one entry per run of consecutive
    /// holdings.
    pub readings: Vec<(f64, f64, String)>,
}

/// One validator decision, calibrated.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionFact {
    pub call: String,
    pub freq_hz: f64,
    pub t_s: f64,
    /// `None` for a spotted decision.
    pub reason: Option<Reason>,
    pub reps: Option<u32>,
    pub required_reps: u32,
}

/// One emitted spot, calibrated.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotFact {
    pub call: String,
    pub freq_hz: f64,
    pub t_s: f64,
    pub track_id: u32,
    pub sample_ts: u64,
    /// `cq`, `de`, `beacon` or `unknown`.
    pub spot_type: String,
}

/// Rises refused on one channel in one stream second, for one reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VetoFact {
    /// The start of the stream second the rises fell in.
    pub t_s: f64,
    /// The channel centre, calibrated.
    pub freq_hz: f64,
    pub veto: SpawnVeto,
    pub count: u32,
}

/// Everything the production pass left for the funnel to read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evidence {
    pub tracks: Vec<TrackFacts>,
    pub decisions: Vec<DecisionFact>,
    pub spots: Vec<SpotFact>,
    /// The spawn-veto census: rises that started no candidate track.
    pub vetoes: Vec<VetoFact>,
}

/// The most-counted veto among `facts` that `keep` admits; ties go to the
/// first in `SpawnVeto` order, so the choice is deterministic.
fn top_veto(facts: &[&VetoFact], keep: impl Fn(SpawnVeto) -> bool) -> Option<SpawnVeto> {
    let mut counts: BTreeMap<SpawnVeto, u64> = BTreeMap::new();
    for v in facts.iter().filter(|v| keep(v.veto)) {
        *counts.entry(v.veto).or_insert(0) += u64::from(v.count);
    }
    counts
        .into_iter()
        .rev()
        .max_by_key(|&(_, n)| n)
        .map(|(veto, _)| veto)
}

/// Per-opportunity track counts: candidate births, confirmations and
/// active-track (leased) births stay apart.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackTally {
    pub candidates: usize,
    pub confirmed: usize,
    pub leased: usize,
    pub track_ids: Vec<u32>,
}

/// One spot a record cites.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSpot {
    pub track_id: u32,
    pub sample_ts: u64,
    pub spot_type: String,
}

/// One `lossFunnel` line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub kind: &'static str,
    pub call: String,
    pub frequency_hz: f64,
    pub first_s: f64,
    pub last_s: f64,
    pub rows: usize,
    pub snr_db: i32,
    pub wpm: Option<f32>,
    /// The acoustic window: [first - W/2, last + W/2].
    pub window_s: [f64; 2],
    /// The call-keyed evidence window: [first - S, last + S].
    pub spot_window_s: [f64; 2],
    /// The acoustic window clipped to the recording; `None` outside it.
    pub covered_s: Option<[f64; 2]>,
    /// The furthest stage with evidence; `None` when input coverage failed.
    pub reached: Option<Stage>,
    /// The earliest failed stage; `None` when delivered.
    pub lost_at: Option<Stage>,
    pub detail: Option<String>,
    pub tracks: TrackTally,
    pub speed_wpm: Option<f32>,
    pub track_text: Option<String>,
    /// Each distinct validator outcome for the call, in first-seen order.
    pub decisions: Vec<String>,
    pub spots: Vec<RecordSpot>,
}

/// The most characters a record's `trackText` carries.
pub const TRACK_TEXT_CHARS: usize = 200;

/// What a withhold reason proves about the funnel. Exhaustive, so a new
/// `Reason` fails to compile until someone places it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasonEffect {
    /// Lost at self-ID recognition.
    RejectsSelfId,
    /// Passes self-ID, lost at sufficient evidence.
    LacksEvidence,
    /// An operator blocklist or notch: checked ahead of every validation
    /// gate (`manta-spot` validator, MAN-31), so it proves the call reached
    /// the validator and nothing about self-ID or evidence.
    PolicyGate,
    /// Passes evidence, lost at emission.
    Withholds,
    /// A duplicate of an earlier spot: passes evidence. It credits emission
    /// only through a spot that itself matches the opportunity, since the
    /// dedupe window outlasts the opportunity's spot window.
    Duplicate,
}

fn reason_effect(r: Reason) -> ReasonEffect {
    match r {
        Reason::Grammar | Reason::Cty | Reason::PowerStepGuard | Reason::BeaconNoEvidence => {
            ReasonEffect::RejectsSelfId
        }
        Reason::Repetition
        | Reason::Variant
        | Reason::NoEvidence
        | Reason::BeaconPending
        | Reason::BeaconSpeed
        | Reason::BeaconOverflow
        | Reason::BeaconDiscarded => ReasonEffect::LacksEvidence,
        Reason::Blocklist | Reason::Notch => ReasonEffect::PolicyGate,
        Reason::LookupPending | Reason::LookupNotFound => ReasonEffect::Withholds,
        Reason::Dedupe => ReasonEffect::Duplicate,
    }
}

/// `manta grade`'s speed rule (crates/manta-server/src/rbn_compare/grade.rs):
/// within max(3 WPM, 20%) of the truth.
fn speed_ok(est: f32, truth: f32) -> bool {
    (est - truth).abs() <= (0.2 * truth).max(3.0)
}

fn overlaps(born_s: f64, end_s: Option<f64>, w: [f64; 2]) -> bool {
    born_s <= w[1] && end_s.unwrap_or(f64::INFINITY) >= w[0]
}

/// Charges `op` to exactly one earliest failed stage, or none when a spot
/// for its call was delivered. Later evidence implies the earlier stages,
/// so an opportunity is never counted at more than one.
pub fn attribute(op: &Opportunity, ev: &Evidence, geo: &Geometry, tol: &Tolerances) -> Record {
    let f = op.freq_hz;
    let window_s = [
        op.first_s - tol.window_s / 2.0,
        op.last_s + tol.window_s / 2.0,
    ];
    let spot_window_s = [
        op.first_s - tol.spot_window_s,
        op.last_s + tol.spot_window_s,
    ];
    let (lo, hi) = (window_s[0].max(0.0), window_s[1].min(geo.duration_s));
    let covered_s = (lo < hi).then_some([lo, hi]);
    let mut rec = Record {
        kind: "lossFunnel",
        call: op.call.clone(),
        frequency_hz: f,
        first_s: op.first_s,
        last_s: op.last_s,
        rows: op.rows,
        snr_db: op.snr_db,
        wpm: op.wpm,
        window_s,
        spot_window_s,
        covered_s,
        reached: None,
        lost_at: Some(Stage::InputCoverage),
        detail: None,
        tracks: TrackTally::default(),
        speed_wpm: None,
        track_text: None,
        decisions: Vec::new(),
        spots: Vec::new(),
    };

    // Stage 1: input coverage.
    let half_ch = geo.channel_width_hz / 2.0;
    if covered_s.is_none() {
        rec.detail = Some("outsideRecording".into());
        return rec;
    }
    if f < geo.passband_hz.0 + half_ch || f > geo.passband_hz.1 - half_ch {
        rec.detail = Some("outsidePassband".into());
        return rec;
    }

    // Stages 2-4: call-agnostic track evidence near the frequency.
    let track_tol = tol.track_tol_hz + half_ch;
    let candidates: Vec<&TrackFacts> = ev
        .tracks
        .iter()
        .filter(|t| {
            ((t.spawn_freq_hz - f).abs() <= track_tol || (t.freq_hz - f).abs() <= track_tol)
                && overlaps(t.born_s, t.end_s, window_s)
        })
        .collect();
    // Rises near the frequency, inside the window, that spawned nothing.
    let vetoes: Vec<&VetoFact> = ev
        .vetoes
        .iter()
        .filter(|v| {
            (v.freq_hz - f).abs() <= track_tol && v.t_s + 1.0 > window_s[0] && v.t_s <= window_s[1]
        })
        .collect();
    let leased: Vec<&TrackFacts> = candidates.iter().copied().filter(|t| t.leased).collect();
    rec.tracks = TrackTally {
        candidates: candidates.len(),
        confirmed: candidates.iter().filter(|t| t.confirmed).count(),
        leased: leased.len(),
        track_ids: leased.iter().map(|t| t.track_id).collect(),
    };
    let speeds: Vec<f32> = leased
        .iter()
        .flat_map(|t| {
            let inside: Vec<f32> = t
                .speeds
                .iter()
                .filter(|(s, _)| *s >= window_s[0] && *s <= window_s[1])
                .map(|&(_, w)| w)
                .collect();
            if inside.is_empty() {
                t.speeds.iter().map(|&(_, w)| w).collect()
            } else {
                inside
            }
        })
        .collect();
    let timing_ok = match op.wpm {
        Some(truth) => speeds.iter().any(|&w| speed_ok(w, truth)),
        None => !speeds.is_empty(),
    };
    rec.speed_wpm = match op.wpm {
        Some(truth) => speeds
            .iter()
            .copied()
            .min_by(|a, b| (a - truth).abs().total_cmp(&(b - truth).abs())),
        None => speeds.last().copied(),
    };

    // Stages 5-9: call-keyed evidence within the spot tolerances.
    let near = |freq: f64, t: f64| {
        (freq - f).abs() <= tol.spot_tol_hz && t >= spot_window_s[0] && t <= spot_window_s[1]
    };
    let call = op.call.as_str();
    let near_tracks: Vec<&TrackFacts> = ev
        .tracks
        .iter()
        .filter(|t| {
            t.leased
                && ((t.freq_hz - f).abs() <= tol.spot_tol_hz
                    || (t.spawn_freq_hz - f).abs() <= tol.spot_tol_hz)
                && overlaps(t.born_s, t.end_s, spot_window_s)
        })
        .collect();
    // Only a word decoded, or a reading held, inside the spot window counts:
    // a long-lived track's text from outside it says nothing about this
    // opportunity.
    let in_spot_window =
        |w: &&(f64, f64, String)| w.0 <= spot_window_s[1] && w.1 >= spot_window_s[0];
    let call_track = near_tracks.iter().find(|t| {
        t.words
            .iter()
            .chain(&t.readings)
            .filter(in_spot_window)
            .any(|(_, _, w)| w == call)
    });
    let text_ok = call_track.is_some();
    let shown = call_track.or(leased.first()).or(near_tracks.first());
    rec.track_text = shown.map(|t| t.text.chars().take(TRACK_TEXT_CHARS).collect());

    let decisions: Vec<&DecisionFact> = ev
        .decisions
        .iter()
        .filter(|d| d.call == call && near(d.freq_hz, d.t_s))
        .collect();
    for d in &decisions {
        let label = d.reason.map_or("spotted", Reason::label).to_string();
        if !rec.decisions.contains(&label) {
            rec.decisions.push(label);
        }
    }
    let spots: Vec<&SpotFact> = ev
        .spots
        .iter()
        .filter(|s| s.call == call && near(s.freq_hz, s.t_s))
        .collect();
    rec.spots = spots
        .iter()
        .map(|s| RecordSpot {
            track_id: s.track_id,
            sample_ts: s.sample_ts,
            spot_type: s.spot_type.clone(),
        })
        .collect();
    let effect = |d: &&DecisionFact| d.reason.map(reason_effect);
    let self_id_ok = decisions.iter().any(|d| {
        !matches!(
            effect(d),
            Some(ReasonEffect::RejectsSelfId | ReasonEffect::PolicyGate)
        )
    });
    let evidence_ok = decisions.iter().any(|d| {
        matches!(
            effect(d),
            None | Some(ReasonEffect::Withholds | ReasonEffect::Duplicate)
        )
    });
    // Only a spot inside the opportunity's own tolerances proves emission:
    // a dedupe withhold's original may lie outside them.
    let emitted_ok = !spots.is_empty();

    let evidence = [
        true,
        !candidates.is_empty(),
        !leased.is_empty(),
        timing_ok,
        text_ok,
        self_id_ok,
        evidence_ok,
        emitted_ok,
        // Offline, delivered equals emitted: the spot bus drops nothing.
        emitted_ok,
    ];
    let reached = Stage::ALL
        .iter()
        .copied()
        .rev()
        .find(|s| evidence[*s as usize])
        .unwrap_or(Stage::InputCoverage);
    // Stage 1, source masks: nothing later passed and every rise near the
    // frequency was refused by the input's own mask.
    if reached == Stage::InputCoverage
        && !vetoes.is_empty()
        && vetoes.iter().all(|v| v.veto.is_source_mask())
    {
        let mask =
            top_veto(&vetoes, SpawnVeto::is_source_mask).expect("non-empty, all source masks");
        rec.detail = Some(format!("sourceMask:{}", mask.label()));
        return rec;
    }
    rec.reached = Some(reached);
    rec.lost_at = reached.next();
    rec.detail = rec.lost_at.map(|stage| match stage {
        Stage::InputCoverage => unreachable!("input coverage passed above"),
        Stage::DetectableEnergy => match top_veto(&vetoes, |v| !v.is_source_mask()) {
            Some(v) => format!("refusedRise:{}", v.label()),
            None => "noRise".to_string(),
        },
        Stage::AdmittedTrack => {
            // The most advanced candidate: confirmed first, then the latest.
            let best = candidates
                .iter()
                .max_by(|a, b| {
                    (a.confirmed, a.born_s)
                        .partial_cmp(&(b.confirmed, b.born_s))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .expect("admittedTrack is lost only with a candidate");
            match best.close {
                Some(r) => format!("closed:{}", r.label()),
                None => "openAtEnd".to_string(),
            }
        }
        Stage::UsableTiming => {
            if speeds.is_empty() {
                "noSpeedUpdate".to_string()
            } else {
                "speedOff".to_string()
            }
        }
        Stage::TextContainsCall => {
            if near_tracks.iter().any(|t| {
                t.words
                    .iter()
                    .filter(in_spot_window)
                    .map(|(_, _, w)| w.as_str())
                    .collect::<String>()
                    .contains(call)
            }) {
                "substringOnly".to_string()
            } else {
                "noCall".to_string()
            }
        }
        Stage::SelfIdRecognized => match decisions.last().and_then(|d| d.reason) {
            Some(r @ (Reason::Blocklist | Reason::Notch)) => format!("policy:{}", r.label()),
            Some(r) => format!("rejected:{}", r.label()),
            None => "noDecision".to_string(),
        },
        Stage::SufficientEvidence => decisions
            .iter()
            .rev()
            .find(|d| effect(d) == Some(ReasonEffect::LacksEvidence))
            .map(|d| {
                let r = d.reason.expect("a lacking-evidence decision has a reason");
                format!("{} {}/{}", r.label(), d.reps.unwrap_or(0), d.required_reps)
            })
            .unwrap_or_else(|| "noDecision".to_string()),
        Stage::Emitted => decisions
            .iter()
            .rev()
            .find_map(|d| match d.reason {
                Some(Reason::Blocklist) => Some("policy:blocklist"),
                Some(Reason::Notch) => Some("policy:notch"),
                Some(Reason::LookupPending) => Some("lookupPending"),
                Some(Reason::LookupNotFound) => Some("lookupNotFound"),
                Some(Reason::Dedupe) => Some("dedupeOutsideWindow"),
                _ => None,
            })
            .unwrap_or("notEmitted")
            .to_string(),
        Stage::Delivered => "notDelivered".to_string(),
    });
    rec
}

/// Counts per stage, in funnel order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageCounts {
    pub input_coverage: usize,
    pub detectable_energy: usize,
    pub admitted_track: usize,
    pub usable_timing: usize,
    pub text_contains_call: usize,
    pub self_id_recognized: usize,
    pub sufficient_evidence: usize,
    pub emitted: usize,
    pub delivered: usize,
}

impl StageCounts {
    fn slot(&mut self, stage: Stage) -> &mut usize {
        match stage {
            Stage::InputCoverage => &mut self.input_coverage,
            Stage::DetectableEnergy => &mut self.detectable_energy,
            Stage::AdmittedTrack => &mut self.admitted_track,
            Stage::UsableTiming => &mut self.usable_timing,
            Stage::TextContainsCall => &mut self.text_contains_call,
            Stage::SelfIdRecognized => &mut self.self_id_recognized,
            Stage::SufficientEvidence => &mut self.sufficient_evidence,
            Stage::Emitted => &mut self.emitted,
            Stage::Delivered => &mut self.delivered,
        }
    }

    /// The count for `stage`.
    pub fn get(&self, stage: Stage) -> usize {
        let mut copy = *self;
        *copy.slot(stage)
    }
}

/// Run-wide track births: candidates, confirmations and leased (active)
/// tracks, kept apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Births {
    pub candidates: usize,
    pub confirmed: usize,
    pub leased: usize,
}

/// Run-wide closes by reason, plus the tracks still open at the end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseTally {
    pub unconfirmed: usize,
    pub hang_expired: usize,
    pub silent: usize,
    pub merged: usize,
    pub evicted: usize,
    pub rtty: usize,
    pub not_cw: usize,
    pub unkeyed: usize,
    pub open_at_end: usize,
}

impl CloseTally {
    pub(crate) fn record(&mut self, r: TrackCloseReason) {
        let slot = match r {
            TrackCloseReason::Unconfirmed => &mut self.unconfirmed,
            TrackCloseReason::HangExpired => &mut self.hang_expired,
            TrackCloseReason::Silent => &mut self.silent,
            TrackCloseReason::Merged => &mut self.merged,
            TrackCloseReason::Evicted => &mut self.evicted,
            TrackCloseReason::Rtty => &mut self.rtty,
            TrackCloseReason::NotCw => &mut self.not_cw,
            TrackCloseReason::Unkeyed => &mut self.unkeyed,
        };
        *slot += 1;
    }
}

/// The `lossFunnelSummary` line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub kind: &'static str,
    pub opportunities: usize,
    pub delivered: usize,
    pub lost_at: StageCounts,
    /// Opportunities whose `reached` is at or past each stage.
    pub passed: StageCounts,
    pub delivered_by_spot_type: BTreeMap<String, usize>,
    /// `[bucket, opportunities, delivered]`, the oracle's SNR buckets.
    pub by_snr: Vec<(String, usize, usize)>,
    pub births: Births,
    pub closes: CloseTally,
}

fn snr_bucket(snr: i32) -> &'static str {
    if snr < 15 {
        "lt15"
    } else if snr < 25 {
        "15-25"
    } else {
        "ge25"
    }
}

/// Sums `records`. `sum(lost_at) + delivered == opportunities` always.
pub fn summarize(records: &[Record], births: Births, closes: CloseTally) -> Summary {
    let mut lost_at = StageCounts::default();
    let mut passed = StageCounts::default();
    let mut delivered = 0;
    let mut by_type = BTreeMap::<String, usize>::new();
    let mut by_snr = BTreeMap::<&str, (usize, usize)>::new();
    for r in records {
        let bucket = by_snr.entry(snr_bucket(r.snr_db)).or_default();
        bucket.0 += 1;
        match r.lost_at {
            Some(stage) => *lost_at.slot(stage) += 1,
            None => {
                delivered += 1;
                bucket.1 += 1;
                let kind = r
                    .spots
                    .first()
                    .map_or("deliveredEarlier", |s| s.spot_type.as_str());
                *by_type.entry(kind.to_string()).or_default() += 1;
            }
        }
        if let Some(reached) = r.reached {
            for stage in Stage::ALL.iter().copied().filter(|s| *s <= reached) {
                *passed.slot(stage) += 1;
            }
        }
    }
    Summary {
        kind: "lossFunnelSummary",
        opportunities: records.len(),
        delivered,
        lost_at,
        passed,
        delivered_by_spot_type: by_type,
        by_snr: by_snr
            .into_iter()
            .map(|(k, (n, d))| (k.to_string(), n, d))
            .collect(),
        births,
        closes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: f64 = 14_003_000.0;

    fn geo() -> Geometry {
        Geometry {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 14_000_000.0,
            passband_hz: (13_952_000.0, 14_048_000.0),
            channel_width_hz: 93.75,
            duration_s: 60.0,
            freq_correction_ppm: 0.0,
        }
    }

    fn op(call: &str, freq_hz: f64, t: f64) -> Opportunity {
        Opportunity {
            call: call.into(),
            freq_hz,
            first_s: t,
            last_s: t,
            rows: 1,
            snr_db: 25,
            wpm: Some(20.0),
        }
    }

    fn track(id: u32, freq_hz: f64) -> TrackFacts {
        TrackFacts {
            track_id: id,
            spawn_freq_hz: freq_hz,
            freq_hz,
            born_s: 5.0,
            end_s: None,
            confirmed: true,
            leased: true,
            close: None,
            speeds: vec![(20.0, 20.0)],
            text: String::new(),
            words: Vec::new(),
            readings: Vec::new(),
        }
    }

    /// Sets `t`'s text, every word decoded at `at` seconds.
    fn says(t: &mut TrackFacts, text: &str, at: f64) {
        t.text = text.into();
        t.words = text
            .split_whitespace()
            .map(|w| (at, at, w.to_string()))
            .collect();
    }

    fn decision(call: &str, reason: Option<Reason>, reps: u32) -> DecisionFact {
        DecisionFact {
            call: call.into(),
            freq_hz: F,
            t_s: 30.0,
            reason,
            reps: Some(reps),
            required_reps: 2,
        }
    }

    fn spot(call: &str, freq_hz: f64) -> SpotFact {
        SpotFact {
            call: call.into(),
            freq_hz,
            t_s: 30.0,
            track_id: 1,
            sample_ts: 2_880_000,
            spot_type: "cq".into(),
        }
    }

    fn run(op: &Opportunity, ev: &Evidence) -> Record {
        attribute(op, ev, &geo(), &Tolerances::default())
    }

    fn lost(r: &Record) -> (Option<Stage>, Option<&str>) {
        (r.lost_at, r.detail.as_deref())
    }

    fn veto(freq_hz: f64, t_s: f64, veto: SpawnVeto, count: u32) -> VetoFact {
        VetoFact {
            t_s,
            freq_hz,
            veto,
            count,
        }
    }

    #[test]
    fn refused_rise_names_the_most_counted_detector_veto() {
        let ev = Evidence {
            vetoes: vec![
                veto(F + 50.0, 20.0, SpawnVeto::CoolingDown, 5),
                veto(F, 21.0, SpawnVeto::NotLocalPeak, 2),
                // A source mask alongside: the detector still refused rises.
                veto(F, 22.0, SpawnVeto::AnalyticGuard, 9),
                // Outside the track tolerance and outside the window.
                veto(F + 1000.0, 20.0, SpawnVeto::OwnedByTrack, 50),
                veto(F, 200.0, SpawnVeto::Rtty, 50),
            ],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, Some(Stage::InputCoverage));
        assert_eq!(
            lost(&r),
            (
                Some(Stage::DetectableEnergy),
                Some("refusedRise:coolingDown")
            )
        );
    }

    #[test]
    fn source_mask_only_is_input_coverage() {
        let ev = Evidence {
            vetoes: vec![
                veto(F, 20.0, SpawnVeto::AnalyticGuard, 7),
                veto(F + 90.0, 21.0, SpawnVeto::MirrorImage, 3),
            ],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, None);
        assert_eq!(
            lost(&r),
            (Some(Stage::InputCoverage), Some("sourceMask:analyticGuard"))
        );
        let s = summarize(&[r], Births::default(), CloseTally::default());
        assert_eq!(s.lost_at.input_coverage, 1);
        assert_eq!(s.lost_at.detectable_energy, 0);
    }

    #[test]
    fn source_mask_does_not_hide_later_evidence() {
        // A spot for the call outweighs a masked rise near it.
        let ev = Evidence {
            spots: vec![spot("W1AW", F + 400.0)],
            vetoes: vec![veto(F, 20.0, SpawnVeto::TrustedBand, 4)],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, Some(Stage::Delivered));
        assert_eq!(r.lost_at, None);
    }

    #[test]
    fn gherkin_no_track_opened_is_detectable_energy() {
        // Later-stage-looking facts for another call on another frequency.
        let mut t = track(1, 14_010_000.0);
        says(&mut t, "CQ DE K1ABC K", 30.0);
        let mut d = decision("K1ABC", None, 2);
        d.freq_hz = 14_010_000.0;
        let ev = Evidence {
            tracks: vec![t],
            decisions: vec![d],
            spots: vec![spot("K1ABC", 14_010_000.0)],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, Some(Stage::InputCoverage));
        assert_eq!(lost(&r), (Some(Stage::DetectableEnergy), Some("noRise")));
        assert_eq!(r.tracks, TrackTally::default());
        let s = summarize(&[r], Births::default(), CloseTally::default());
        assert_eq!(s.lost_at.detectable_energy, 1);
        let others: usize = Stage::ALL
            .iter()
            .filter(|st| **st != Stage::DetectableEnergy)
            .map(|st| s.lost_at.get(*st))
            .sum();
        assert_eq!(others, 0);
        assert_eq!(s.delivered, 0);
    }

    #[test]
    fn gherkin_validated_spot_is_delivered() {
        let mut t = track(1, F);
        says(&mut t, "CQ DE W1AW W1AW K", 30.0);
        let ev = Evidence {
            tracks: vec![t],
            decisions: vec![decision("W1AW", None, 2)],
            spots: vec![spot("W1AW", F)],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, Some(Stage::Delivered));
        assert_eq!(lost(&r), (None, None));
        let s = summarize(&[r], Births::default(), CloseTally::default());
        assert_eq!(s.delivered, 1);
        assert_eq!(s.passed.delivered, 1);
        assert_eq!(s.delivered_by_spot_type.get("cq"), Some(&1));
    }

    #[test]
    fn outside_recording_and_passband_are_input_coverage() {
        let ev = Evidence::default();
        let r = run(&op("W9LATE", F, 660.0), &ev);
        assert_eq!(
            lost(&r),
            (Some(Stage::InputCoverage), Some("outsideRecording"))
        );
        assert_eq!(r.reached, None);
        assert_eq!(r.covered_s, None);
        let r = run(&op("VE3OUT", 14_060_000.0, 30.0), &ev);
        assert_eq!(
            lost(&r),
            (Some(Stage::InputCoverage), Some("outsidePassband"))
        );
    }

    #[test]
    fn closed_not_cw_names_the_most_advanced_candidate() {
        let mut a = track(1, F);
        a.leased = false;
        a.confirmed = false;
        a.close = Some(TrackCloseReason::Unconfirmed);
        let mut b = track(2, F + 40.0);
        b.leased = false;
        b.close = Some(TrackCloseReason::NotCw);
        let ev = Evidence {
            tracks: vec![b, a],
            ..Default::default()
        };
        let r = run(&op("F5ABC", F, 30.0), &ev);
        assert_eq!(lost(&r), (Some(Stage::AdmittedTrack), Some("closed:notCw")));
        assert_eq!(
            (r.tracks.candidates, r.tracks.confirmed, r.tracks.leased),
            (2, 1, 0)
        );
    }

    #[test]
    fn open_at_end_when_the_candidate_never_closed() {
        let mut a = track(1, F);
        a.leased = false;
        let r = run(
            &op("F5ABC", F, 30.0),
            &Evidence {
                tracks: vec![a],
                ..Default::default()
            },
        );
        assert_eq!(lost(&r), (Some(Stage::AdmittedTrack), Some("openAtEnd")));
    }

    #[test]
    fn timing_details_and_missing_truth_speed() {
        let mut t = track(1, F);
        t.speeds = vec![(30.0, 14.97)];
        let ev = Evidence {
            tracks: vec![t.clone()],
            ..Default::default()
        };
        let mut o = op("G3ABC", F, 30.0);
        o.wpm = Some(40.0);
        let r = run(&o, &ev);
        assert_eq!(lost(&r), (Some(Stage::UsableTiming), Some("speedOff")));
        assert_eq!(r.speed_wpm, Some(14.97));
        o.wpm = None;
        let r = run(&o, &ev);
        assert_eq!(lost(&r), (Some(Stage::TextContainsCall), Some("noCall")));
        t.speeds.clear();
        let r = run(
            &op("G3ABC", F, 30.0),
            &Evidence {
                tracks: vec![t],
                ..Default::default()
            },
        );
        assert_eq!(lost(&r), (Some(Stage::UsableTiming), Some("noSpeedUpdate")));
    }

    #[test]
    fn text_details_and_held_readings() {
        let mut t = track(1, F);
        says(&mut t, "CQ DE G4X YZ K", 30.0);
        let ev = Evidence {
            tracks: vec![t.clone()],
            ..Default::default()
        };
        let r = run(&op("G4XYZ", F, 30.0), &ev);
        assert_eq!(
            lost(&r),
            (Some(Stage::TextContainsCall), Some("substringOnly"))
        );
        t.readings = vec![(30.0, 30.0, "G4XYZ".into())];
        let r = run(
            &op("G4XYZ", F, 30.0),
            &Evidence {
                tracks: vec![t],
                ..Default::default()
            },
        );
        assert_eq!(
            lost(&r),
            (Some(Stage::SelfIdRecognized), Some("noDecision"))
        );
    }

    #[test]
    fn call_decoded_outside_the_spot_window_is_not_text_evidence() {
        // Codex review, PR #493: a track alive 0-600 s that decodes W1AW
        // only at 500 s says nothing about a W1AW opportunity at 100 s,
        // whose spot window is [10, 190] s.
        let geo = Geometry {
            duration_s: 600.0,
            ..geo()
        };
        let at_100 =
            |ev: &Evidence| attribute(&op("W1AW", F, 100.0), ev, &geo, &Tolerances::default());
        let mut t = track(1, F);
        t.born_s = 0.0;
        t.end_s = Some(600.0);
        t.speeds = vec![(100.0, 20.0)];
        says(&mut t, "CQ DE W1 AW K W1AW K", 500.0);
        t.readings = vec![(500.0, 500.0, "W1AW".into())];
        let ev = Evidence {
            tracks: vec![t.clone()],
            ..Default::default()
        };
        let r = at_100(&ev);
        assert_eq!(r.reached, Some(Stage::UsableTiming));
        assert_eq!(lost(&r), (Some(Stage::TextContainsCall), Some("noCall")));

        // The same call held as a reading inside the window passes.
        let mut held = t.clone();
        held.readings.push((150.0, 151.0, "W1AW".into()));
        let r = at_100(&Evidence {
            tracks: vec![held],
            ..Default::default()
        });
        assert_eq!(
            lost(&r),
            (Some(Stage::SelfIdRecognized), Some("noDecision"))
        );

        // A word spanning the window's edge counts; split words in the
        // window are only a substring.
        let mut edge = t.clone();
        edge.words.push((185.0, 195.0, "W1AW".into()));
        let r = at_100(&Evidence {
            tracks: vec![edge],
            ..Default::default()
        });
        assert_eq!(r.lost_at, Some(Stage::SelfIdRecognized));
        let mut split = t;
        split.words.push((120.0, 121.0, "W1".into()));
        split.words.push((122.0, 123.0, "AW".into()));
        let r = at_100(&Evidence {
            tracks: vec![split],
            ..Default::default()
        });
        assert_eq!(
            lost(&r),
            (Some(Stage::TextContainsCall), Some("substringOnly"))
        );
    }

    #[test]
    fn decision_reasons_place_the_loss() {
        let mut t = track(1, F);
        says(&mut t, "CQ DE JA1ZZZ K", 30.0);
        let at = |d: DecisionFact| {
            let ev = Evidence {
                tracks: vec![t.clone()],
                decisions: vec![d],
                spots: vec![],
                ..Default::default()
            };
            let r = run(&op("JA1ZZZ", F, 30.0), &ev);
            (r.lost_at, r.detail)
        };
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::Cty), 1)),
            (Some(Stage::SelfIdRecognized), Some("rejected:cty".into()))
        );
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::Repetition), 1)),
            (
                Some(Stage::SufficientEvidence),
                Some("repetition 1/2".into())
            )
        );
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::NoEvidence), 0)),
            (
                Some(Stage::SufficientEvidence),
                Some("noEvidence 0/2".into())
            )
        );
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::Blocklist), 2)),
            (
                Some(Stage::SelfIdRecognized),
                Some("policy:blocklist".into())
            )
        );
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::LookupNotFound), 2)),
            (Some(Stage::Emitted), Some("lookupNotFound".into()))
        );
        // No spot of its own inside the windows: dedupe proves evidence,
        // not emission (the PR #493 P1).
        assert_eq!(
            at(decision("JA1ZZZ", Some(Reason::Dedupe), 2)),
            (Some(Stage::Emitted), Some("dedupeOutsideWindow".into()))
        );
    }

    /// PR #493 P1: the validator checks blocklist and notch before grammar,
    /// cty, acoustic evidence, repetition and variant (MAN-31), so a policy
    /// withhold credits none of those gates.
    #[test]
    fn policy_rejection_credits_no_unexecuted_gate() {
        let mut t = track(1, F);
        says(&mut t, "CQ DE JA1ZZZ K", 30.0);
        let run_with = |decisions: Vec<DecisionFact>| {
            let ev = Evidence {
                tracks: vec![t.clone()],
                decisions,
                ..Default::default()
            };
            let r = run(&op("JA1ZZZ", F, 30.0), &ev);
            (r.reached, r.lost_at, r.detail)
        };
        // A blocklisted call decoded once, needing two repetitions: neither
        // self-ID nor evidence was ever checked.
        for (reason, detail) in [
            (Reason::Blocklist, "policy:blocklist"),
            (Reason::Notch, "policy:notch"),
        ] {
            assert_eq!(
                run_with(vec![decision("JA1ZZZ", Some(reason), 1)]),
                (
                    Some(Stage::TextContainsCall),
                    Some(Stage::SelfIdRecognized),
                    Some(detail.into())
                ),
                "{reason:?}"
            );
        }
        // A gate that did run still places the loss.
        assert_eq!(
            run_with(vec![
                decision("JA1ZZZ", Some(Reason::Repetition), 1),
                decision("JA1ZZZ", Some(Reason::Blocklist), 1),
            ]),
            (
                Some(Stage::SelfIdRecognized),
                Some(Stage::SufficientEvidence),
                Some("repetition 1/2".into())
            )
        );
    }

    /// PR #493 P1: the dedupe window (600 s) outlasts the spot window, so a
    /// dedupe withhold inside the window can point at an original spot
    /// outside it. Only a spot that itself matches the opportunity credits
    /// emission and delivery.
    #[test]
    fn dedupe_credits_delivery_only_through_a_matching_spot() {
        let geo = Geometry {
            duration_s: 600.0,
            ..geo()
        };
        let mut t = track(1, F);
        says(&mut t, "CQ DE JA1ZZZ K", 400.0);
        let dup = DecisionFact {
            t_s: 400.0,
            ..decision("JA1ZZZ", Some(Reason::Dedupe), 2)
        };
        let original_at = |t_s: f64| SpotFact {
            t_s,
            ..spot("JA1ZZZ", F)
        };
        let run_with = |spots: Vec<SpotFact>| {
            let ev = Evidence {
                tracks: vec![t.clone()],
                decisions: vec![dup.clone()],
                spots,
                ..Default::default()
            };
            attribute(&op("JA1ZZZ", F, 400.0), &ev, &geo, &Tolerances::default())
        };
        // The original spot at 10 s, outside [310, 490].
        let r = run_with(vec![original_at(10.0)]);
        assert_eq!(r.spot_window_s, [310.0, 490.0]);
        assert_eq!(
            (r.reached, r.lost_at, r.detail.as_deref()),
            (
                Some(Stage::SufficientEvidence),
                Some(Stage::Emitted),
                Some("dedupeOutsideWindow")
            )
        );
        assert!(r.spots.is_empty());
        // The original inside the window: delivered.
        let r = run_with(vec![original_at(350.0)]);
        assert_eq!((r.lost_at, r.reached), (None, Some(Stage::Delivered)));
    }

    #[test]
    fn every_reason_maps_to_a_stage() {
        let all = [
            Reason::Blocklist,
            Reason::Notch,
            Reason::Grammar,
            Reason::Cty,
            Reason::Repetition,
            Reason::Variant,
            Reason::Dedupe,
            Reason::PowerStepGuard,
            Reason::BeaconNoEvidence,
            Reason::BeaconPending,
            Reason::BeaconSpeed,
            Reason::BeaconOverflow,
            Reason::BeaconDiscarded,
            Reason::NoEvidence,
            Reason::LookupPending,
            Reason::LookupNotFound,
        ];
        for r in all {
            let expected = match r {
                Reason::Grammar
                | Reason::Cty
                | Reason::PowerStepGuard
                | Reason::BeaconNoEvidence => ReasonEffect::RejectsSelfId,
                Reason::Repetition
                | Reason::Variant
                | Reason::NoEvidence
                | Reason::BeaconPending
                | Reason::BeaconSpeed
                | Reason::BeaconOverflow
                | Reason::BeaconDiscarded => ReasonEffect::LacksEvidence,
                Reason::Blocklist | Reason::Notch => ReasonEffect::PolicyGate,
                Reason::LookupPending | Reason::LookupNotFound => ReasonEffect::Withholds,
                Reason::Dedupe => ReasonEffect::Duplicate,
            };
            assert_eq!(reason_effect(r), expected, "{r:?}");
        }
    }

    #[test]
    fn later_evidence_implies_earlier_stages() {
        // A spot 400 Hz away: inside the spot tolerance, outside the track one.
        let ev = Evidence {
            spots: vec![spot("W1AW", F + 400.0)],
            ..Default::default()
        };
        let r = run(&op("W1AW", F, 30.0), &ev);
        assert_eq!(r.reached, Some(Stage::Delivered));
        assert_eq!(r.lost_at, None);
    }

    #[test]
    fn one_track_serves_both_calls_of_a_qso() {
        let mut t = track(1, F);
        says(&mut t, "W1XYZ DE K1ABC K", 30.0);
        let ev = Evidence {
            tracks: vec![t],
            decisions: vec![decision("K1ABC", None, 2)],
            spots: vec![spot("K1ABC", F)],
            ..Default::default()
        };
        let a = run(&op("K1ABC", F, 30.0), &ev);
        let b = run(&op("W1XYZ", F, 30.0), &ev);
        assert_eq!(a.lost_at, None);
        assert_eq!(
            lost(&b),
            (Some(Stage::SelfIdRecognized), Some("noDecision"))
        );
        assert_eq!(a.tracks.track_ids, b.tracks.track_ids);
    }

    #[test]
    fn tolerances_are_inclusive_and_track_tolerance_adds_half_a_channel() {
        let edge = 150.0 + 93.75 / 2.0;
        let mut t = track(1, F + edge);
        t.leased = false;
        let ev = Evidence {
            tracks: vec![t.clone()],
            ..Default::default()
        };
        assert_eq!(run(&op("X1X", F, 30.0), &ev).tracks.candidates, 1);
        t.spawn_freq_hz += 1.0;
        t.freq_hz += 1.0;
        let ev = Evidence {
            tracks: vec![t],
            ..Default::default()
        };
        assert_eq!(run(&op("X1X", F, 30.0), &ev).tracks.candidates, 0);
    }

    #[test]
    fn summary_is_consistent() {
        let mut t = track(1, F);
        says(&mut t, "CQ DE W1AW K", 30.0);
        let ev = Evidence {
            tracks: vec![t],
            decisions: vec![decision("W1AW", None, 2)],
            spots: vec![spot("W1AW", F)],
            ..Default::default()
        };
        let records: Vec<Record> = [
            op("W1AW", F, 30.0),
            op("OH2XYZ", 14_021_000.0, 30.0),
            op("W9LATE", F, 660.0),
            op("G4XYZ", F, 30.0),
        ]
        .iter()
        .map(|o| run(o, &ev))
        .collect();
        let s = summarize(&records, Births::default(), CloseTally::default());
        let lost: usize = Stage::ALL.iter().map(|st| s.lost_at.get(*st)).sum();
        assert_eq!(lost + s.delivered, s.opportunities);
        let passed: Vec<usize> = Stage::ALL.iter().map(|st| s.passed.get(*st)).collect();
        assert!(passed.windows(2).all(|w| w[0] >= w[1]), "{passed:?}");
        assert_eq!(passed[0], 3);
    }
}
