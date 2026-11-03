//! MAN-207: the local observation record. One `Observation` is one record
//! about one decoded callsign occurrence; a later record about the same
//! occurrence supersedes it and never rewrites it. Bounded by a record cap
//! and expired by sample time, never by wall clock. Private to manta: no
//! serde, no wire format. See
//! docs/DECISIONS/2026-11-03-man207-observation-record.md.

use crate::context::SpotType;
use crate::decision::{Decision, Reason, Verdict};
use manta_decode::tree::Glyph;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

/// The most records a `Records` or `Text` log keeps (the MAN-62 LRU size).
pub const DEFAULT_MAX_RECORDS: usize = 20_000;
/// How long, in seconds of stream time, a record is kept.
pub const DEFAULT_RETENTION_SECONDS: f64 = 900.0;
/// The most characters a `TextTrace` keeps of one word.
pub const MAX_TRACE_CHARS: usize = 64;

/// How much the log keeps. Ordered: each level keeps what the one before
/// it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Detail {
    /// Counters only; no record is appended (the default).
    #[default]
    Summary,
    /// Records with calls, reasons, scores and evidence, but no decoded text.
    Records,
    /// Records plus each word's characters, confidences, erasures and
    /// decoder alternatives.
    Text,
}

/// Retention for an `ObservationLog`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObservationPolicy {
    pub detail: Detail,
    pub max_records: usize,
    pub retention_seconds: f64,
}

impl ObservationPolicy {
    /// Counters only.
    pub fn summary() -> Self {
        Self {
            detail: Detail::Summary,
            max_records: DEFAULT_MAX_RECORDS,
            retention_seconds: DEFAULT_RETENTION_SECONDS,
        }
    }

    /// Records without decoded text.
    pub fn records() -> Self {
        Self {
            detail: Detail::Records,
            ..Self::summary()
        }
    }

    /// Records with each word's text trace.
    pub fn text() -> Self {
        Self {
            detail: Detail::Text,
            ..Self::summary()
        }
    }
}

impl Default for ObservationPolicy {
    fn default() -> Self {
        Self::summary()
    }
}

/// One decoded occurrence: a track's `word_seq`-th completed word. Track
/// ids are never reused within a run and word seqs strictly increase per
/// track, so the pair is unique in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationId {
    pub track_id: u32,
    pub word_seq: u64,
}

impl fmt::Display for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}.w{}", self.track_id, self.word_seq)
    }
}

/// Why a record exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The occurrence's first decision.
    Decided,
    /// The same word, evaluated again as a new spot type.
    Reclassified,
    /// A pending beacon or lookup decided later.
    Settled,
    /// A re-decode of the same evidence (`ObservationLog::revise`).
    Refined,
}

/// Where the occurrence came from: the input (MAN-13) and the gap-free
/// segment of it (`ObservationLog::note_discontinuity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Source {
    pub input: u32,
    pub epoch: u32,
}

/// The word's sample interval: its first character to its word boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpan {
    pub start_ts: u64,
    pub end_ts: u64,
}

/// The lowest and highest calibrated track frequency read while the word
/// was decoded. A span, not a modeled uncertainty.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqSpan {
    pub lo_hz: f64,
    pub hi_hz: f64,
}

/// The decoder's confidence in the word, and the calibrated call
/// confidence once validation computed one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Scores {
    pub word_confidence: Option<f32>,
    pub call_confidence: Option<f32>,
}

/// One occurrence the repetition gate held for this call when it decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceLink {
    pub observation: ObservationId,
    pub sample_ts: u64,
    /// Whether the gate counted it as a separate message.
    pub message_distinct: bool,
}

/// The word as decoded (`Detail::Text` only).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextTrace {
    pub text: String,
    pub confidences: Vec<f32>,
    /// Positions of the decoder's `'?'` erasure glyphs.
    pub erasures: Vec<u16>,
    /// The decoder's competing glyphs per position; empty when no position
    /// had any.
    pub alternatives: Vec<Vec<(Glyph, f32)>>,
    /// Set when `MAX_TRACE_CHARS` cut the word.
    pub truncated: bool,
}

impl TextTrace {
    /// Keeps at most `MAX_TRACE_CHARS` characters (and their confidences
    /// and alternatives).
    pub fn new(text: &str, confidences: &[f32], alternatives: &[Vec<(Glyph, f32)>]) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let truncated = chars.len() > MAX_TRACE_CHARS;
        let kept: String = chars.iter().take(MAX_TRACE_CHARS).collect();
        let erasures = kept
            .chars()
            .enumerate()
            .filter(|&(_, c)| c == '?')
            .map(|(i, _)| i as u16)
            .collect();
        let alternatives = if alternatives.iter().any(|a| !a.is_empty()) {
            alternatives.iter().take(MAX_TRACE_CHARS).cloned().collect()
        } else {
            Vec::new()
        };
        Self {
            text: kept,
            confidences: confidences.iter().take(MAX_TRACE_CHARS).copied().collect(),
            erasures,
            alternatives,
            truncated,
        }
    }
}

/// What a caller supplies; the log assigns `seq`, `supersedes` and
/// `recorded_at`.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub id: ObservationId,
    pub cause: Cause,
    pub source: Source,
    pub span: Option<SampleSpan>,
    pub freq: Option<FreqSpan>,
    /// The validator's decision: call, spot type, verdict, reason, reps,
    /// pattern tier and rival (MAN-324).
    pub decision: Decision,
    pub scores: Scores,
    pub evidence: Vec<EvidenceLink>,
    pub trace: Option<TextTrace>,
}

/// One appended record. Never modified after it is appended.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// The log's append counter: the record's own id.
    pub seq: u64,
    /// The `seq` of the retained record about the same occurrence this one
    /// replaces as the latest, if any.
    pub supersedes: Option<u64>,
    /// The stream sample time the record was appended at.
    pub recorded_at: u64,
    pub draft: Draft,
}

/// Decisions so far, by reason (`None` is a spot). Fixed arrays: counting
/// never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationCounts {
    decided: [u64; DECIDED_SLOTS],
}

const DECIDED_SLOTS: usize = 1 + Reason::ALL.len();

impl Default for ObservationCounts {
    fn default() -> Self {
        Self {
            decided: [0; DECIDED_SLOTS],
        }
    }
}

impl ObservationCounts {
    fn slot(reason: Option<Reason>) -> usize {
        match reason {
            None => 0,
            Some(r) => 1 + r.index(),
        }
    }

    /// Decisions with this verdict and reason; 0 for a pair that cannot
    /// occur.
    pub fn decided(&self, verdict: Verdict, reason: Option<Reason>) -> u64 {
        let expected = reason.map_or(Verdict::Spotted, Reason::verdict);
        if verdict == expected {
            self.decided[Self::slot(reason)]
        } else {
            0
        }
    }

    /// Every valid (verdict, reason) pair with its count, in a fixed order:
    /// spotted first, then `Reason::ALL`.
    pub fn iter(&self) -> impl Iterator<Item = (Verdict, Option<Reason>, u64)> + '_ {
        std::iter::once((Verdict::Spotted, None, self.decided[0])).chain(
            Reason::ALL
                .iter()
                .map(|&r| (r.verdict(), Some(r), self.decided[Self::slot(Some(r))])),
        )
    }

    /// Every decision counted.
    pub fn total(&self) -> u64 {
        self.decided.iter().sum()
    }
}

/// Records the log lost or could not link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LossCounts {
    /// Records dropped by retention time.
    pub expired: u64,
    /// Records dropped by the record cap.
    pub evicted: u64,
    /// Revisions whose predecessor was no longer retained.
    pub orphaned_revisions: u64,
    /// Records dropped before `take_appended` returned them.
    pub unsent: u64,
}

/// Why `ObservationLog::revise` appended nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviseError {
    /// No record about that occurrence is retained (never recorded,
    /// expired, evicted, or the log keeps no records).
    NotRetained,
}

/// The append-only, bounded record.
#[derive(Debug, Clone)]
pub struct ObservationLog {
    policy: ObservationPolicy,
    retention_samples: u64,
    records: VecDeque<Observation>,
    /// The latest retained record's `seq` per occurrence; bounded by the
    /// deque, since an entry leaves with the record it points at.
    latest: BTreeMap<ObservationId, u64>,
    next_seq: u64,
    sent_through: u64,
    last_recorded_at: u64,
    counts: ObservationCounts,
    losses: LossCounts,
    epoch: u32,
}

impl ObservationLog {
    /// A log at `fs` samples per second.
    pub fn new(fs: f64, policy: ObservationPolicy) -> Self {
        let retention_samples = (policy.retention_seconds.max(0.0) * fs.max(0.0)) as u64;
        Self {
            policy,
            retention_samples,
            records: VecDeque::new(),
            latest: BTreeMap::new(),
            next_seq: 0,
            sent_through: 0,
            last_recorded_at: 0,
            counts: ObservationCounts::default(),
            losses: LossCounts::default(),
            epoch: 0,
        }
    }

    pub fn detail(&self) -> Detail {
        self.policy.detail
    }

    /// Whether `append` keeps records (`Records` or `Text`).
    pub fn keeps_records(&self) -> bool {
        self.policy.detail >= Detail::Records
    }

    /// Appends `draft` as a new record and returns its `seq`; `None` at
    /// `Detail::Summary`. A retained earlier record about the same
    /// occurrence is superseded, never changed. At `Records` any trace is
    /// dropped.
    pub fn append(&mut self, mut draft: Draft, now_ts: u64) -> Option<u64> {
        if !self.keeps_records() {
            return None;
        }
        if self.policy.detail < Detail::Text {
            draft.trace = None;
        }
        let supersedes = self.latest.get(&draft.id).copied();
        if supersedes.is_none() && draft.cause != Cause::Decided {
            self.losses.orphaned_revisions += 1;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        // Records stay in `recorded_at` order, so expiry pops the front.
        self.last_recorded_at = self.last_recorded_at.max(now_ts);
        self.latest.insert(draft.id, seq);
        self.records.push_back(Observation {
            seq,
            supersedes,
            recorded_at: self.last_recorded_at,
            draft,
        });
        while self.records.len() > self.policy.max_records {
            self.drop_front();
            self.losses.evicted += 1;
        }
        Some(seq)
    }

    /// Records a refinement of a retained occurrence as a `Refined`
    /// record linked to the latest one. Revising with content equal to the
    /// latest record's appends nothing and returns that record's `seq`.
    /// Never touches the repetition gate: a re-decode adds no repetition.
    pub fn revise(
        &mut self,
        id: ObservationId,
        mut draft: Draft,
        now_ts: u64,
    ) -> Result<u64, ReviseError> {
        draft.id = id;
        draft.cause = Cause::Refined;
        let Some(&latest) = self.latest.get(&id) else {
            self.losses.orphaned_revisions += 1;
            return Err(ReviseError::NotRetained);
        };
        if let Some(prev) = self.get(latest) {
            let mut same = prev.draft.clone();
            same.cause = Cause::Refined;
            if self.policy.detail < Detail::Text {
                draft.trace = None;
            }
            if same == draft {
                return Ok(latest);
            }
        }
        self.append(draft, now_ts).ok_or(ReviseError::NotRetained)
    }

    /// Drops records appended more than the retention time before `now_ts`.
    pub fn sweep(&mut self, now_ts: u64) {
        let cutoff = now_ts.saturating_sub(self.retention_samples);
        while self.records.front().is_some_and(|o| o.recorded_at < cutoff) {
            self.drop_front();
            self.losses.expired += 1;
        }
    }

    fn drop_front(&mut self) {
        let Some(gone) = self.records.pop_front() else {
            return;
        };
        if self.latest.get(&gone.draft.id) == Some(&gone.seq) {
            self.latest.remove(&gone.draft.id);
        }
        if gone.seq >= self.sent_through {
            self.losses.unsent += 1;
        }
    }

    /// The retained record with this `seq`.
    pub fn get(&self, seq: u64) -> Option<&Observation> {
        let first = self.records.front()?.seq;
        let at = usize::try_from(seq.checked_sub(first)?).ok()?;
        self.records.get(at).filter(|o| o.seq == seq)
    }

    /// The retained records about `track_id`'s occurrences, in `seq` order.
    pub fn for_track(&self, track_id: u32) -> impl Iterator<Item = &Observation> {
        self.records
            .iter()
            .filter(move |o| o.draft.id.track_id == track_id)
    }

    /// The retained records about one occurrence, oldest first.
    pub fn history(&self, id: ObservationId) -> Vec<&Observation> {
        self.records.iter().filter(|o| o.draft.id == id).collect()
    }

    /// Every retained record, in `seq` order.
    pub fn iter(&self) -> impl Iterator<Item = &Observation> {
        self.records.iter()
    }

    /// The records appended since the last call, each returned once.
    pub fn take_appended(&mut self) -> Vec<Observation> {
        // The deque holds consecutive seqs, so the unsent records are its tail.
        let first = self.records.front().map_or(self.next_seq, |o| o.seq);
        let skip = usize::try_from(self.sent_through.saturating_sub(first)).unwrap_or(usize::MAX);
        let out = self.records.iter().skip(skip).cloned().collect();
        self.sent_through = self.next_seq;
        out
    }

    pub fn counts(&self) -> &ObservationCounts {
        &self.counts
    }

    /// Counts one decision. Always on, at every detail level.
    pub fn count_decision(&mut self, reason: Option<Reason>) {
        self.counts.decided[ObservationCounts::slot(reason)] += 1;
    }

    pub fn losses(&self) -> LossCounts {
        self.losses
    }

    /// Starts a new gap-free segment of the stream: later records carry
    /// the next epoch.
    pub fn note_discontinuity(&mut self) {
        self.epoch += 1;
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Draft {
    /// A draft with no spans, scores, evidence or trace.
    pub fn new(id: ObservationId, cause: Cause, source: Source, decision: Decision) -> Self {
        Self {
            id,
            cause,
            source,
            span: None,
            freq: None,
            decision,
            scores: Scores::default(),
            evidence: Vec::new(),
            trace: None,
        }
    }

    /// The candidate call.
    pub fn call(&self) -> &str {
        &self.decision.call
    }

    pub fn spot_type(&self) -> SpotType {
        self.decision.spot_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 1000.0;

    fn id(track_id: u32, word_seq: u64) -> ObservationId {
        ObservationId { track_id, word_seq }
    }

    fn draft(track_id: u32, word_seq: u64, call: &str, reason: Reason) -> Draft {
        let decision = Decision::withheld(
            track_id,
            word_seq * 100,
            call,
            14_025_000.0,
            SpotType::De,
            reason,
        );
        Draft::new(
            id(track_id, word_seq),
            Cause::Decided,
            Source::default(),
            decision,
        )
    }

    fn log(policy: ObservationPolicy) -> ObservationLog {
        ObservationLog::new(FS, policy)
    }

    #[test]
    fn summary_detail_counts_but_appends_nothing() {
        let mut l = log(ObservationPolicy::summary());
        l.count_decision(Some(Reason::Repetition));
        assert_eq!(l.append(draft(1, 2, "K5ARH", Reason::Repetition), 10), None);
        assert_eq!(l.len(), 0);
        assert!(l.is_empty());
        assert_eq!(
            l.counts()
                .decided(Verdict::Withheld, Some(Reason::Repetition)),
            1
        );
    }

    #[test]
    fn first_record_for_an_id_supersedes_nothing() {
        let mut l = log(ObservationPolicy::records());
        assert_eq!(
            l.append(draft(1, 2, "K5ARH", Reason::Repetition), 10),
            Some(0)
        );
        assert_eq!(l.get(0).unwrap().supersedes, None);
        assert_eq!(l.get(0).unwrap().recorded_at, 10);
    }

    #[test]
    fn a_later_record_for_the_same_id_supersedes_the_latest() {
        let mut l = log(ObservationPolicy::records());
        l.append(draft(1, 2, "K5ARH", Reason::BeaconPending), 10);
        let mut settled = draft(1, 2, "K5ARH", Reason::BeaconSpeed);
        settled.cause = Cause::Settled;
        l.append(settled.clone(), 20);
        l.append(settled, 30);
        assert_eq!(l.get(1).unwrap().supersedes, Some(0));
        assert_eq!(l.get(2).unwrap().supersedes, Some(1));
        assert_eq!(l.losses().orphaned_revisions, 0);
    }

    #[test]
    fn a_refinement_with_a_different_call_keeps_the_original() {
        let mut l = log(ObservationPolicy::records());
        let seq = l
            .append(draft(1, 2, "K5ARH", Reason::Repetition), 10)
            .unwrap();
        let original = l.get(seq).unwrap().clone();
        let refined_seq = l
            .revise(id(1, 2), draft(1, 2, "K5ARM", Reason::Repetition), 50)
            .unwrap();
        assert_ne!(refined_seq, seq);
        let refined = l.get(refined_seq).unwrap();
        assert_eq!(refined.supersedes, Some(original.seq));
        assert_eq!(refined.draft.cause, Cause::Refined);
        assert_eq!(refined.draft.call(), "K5ARM");
        assert_eq!(l.get(seq), Some(&original));
        let history: Vec<Observation> = l.history(id(1, 2)).into_iter().cloned().collect();
        assert_eq!(history, vec![original, refined.clone()]);
    }

    #[test]
    fn revising_with_the_latest_content_appends_nothing() {
        let mut l = log(ObservationPolicy::records());
        l.append(draft(1, 2, "K5ARH", Reason::Repetition), 10);
        let first = l
            .revise(id(1, 2), draft(1, 2, "K5ARM", Reason::Repetition), 20)
            .unwrap();
        let again = l
            .revise(id(1, 2), draft(1, 2, "K5ARM", Reason::Repetition), 30)
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn revising_an_unretained_id_is_refused_and_counted() {
        let mut l = log(ObservationPolicy::records());
        assert_eq!(
            l.revise(id(9, 9), draft(9, 9, "K5ARH", Reason::Repetition), 10),
            Err(ReviseError::NotRetained)
        );
        assert_eq!(l.losses().orphaned_revisions, 1);
        assert!(l.is_empty());
    }

    #[test]
    fn records_expire_by_sample_time_only() {
        let policy = ObservationPolicy {
            retention_seconds: 10.0,
            ..ObservationPolicy::records()
        };
        let mut l = log(policy);
        l.append(draft(1, 0, "K5ARH", Reason::Repetition), 1_000);
        l.append(draft(1, 1, "K5ARH", Reason::Repetition), 5_000);
        l.append(draft(1, 2, "K5ARH", Reason::Repetition), 12_000);
        l.sweep(14_999);
        let kept: Vec<u64> = l.iter().map(|o| o.seq).collect();
        assert_eq!(kept, vec![1, 2]);
        assert_eq!(l.losses().expired, 1);
        l.sweep(15_000);
        assert_eq!(l.len(), 2, "recorded_at == cutoff is kept");
        l.sweep(15_001);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn expiry_is_the_same_for_the_same_sample_times() {
        let policy = ObservationPolicy {
            retention_seconds: 3.0,
            ..ObservationPolicy::records()
        };
        let run = || {
            let mut l = log(policy);
            for i in 0..20u64 {
                l.append(
                    draft(1 + (i % 3) as u32, i, "K5ARH", Reason::Dedupe),
                    i * 700,
                );
                l.sweep(i * 700);
            }
            l.iter().cloned().collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
        assert!(!run().is_empty());
    }

    #[test]
    fn the_cap_evicts_oldest_first_and_counts() {
        let policy = ObservationPolicy {
            max_records: 3,
            ..ObservationPolicy::records()
        };
        let mut l = log(policy);
        for i in 0..5 {
            l.append(draft(1, i, "K5ARH", Reason::Repetition), i * 10);
        }
        let kept: Vec<u64> = l.iter().map(|o| o.seq).collect();
        assert_eq!(kept, vec![2, 3, 4]);
        assert_eq!(l.losses().evicted, 2);
    }

    #[test]
    fn an_expired_predecessor_leaves_supersedes_empty() {
        let policy = ObservationPolicy {
            retention_seconds: 1.0,
            ..ObservationPolicy::records()
        };
        let mut l = log(policy);
        l.append(draft(1, 2, "K5ARH", Reason::BeaconPending), 0);
        l.sweep(5_000);
        assert!(l.is_empty());
        let mut settled = draft(1, 2, "K5ARH", Reason::BeaconSpeed);
        settled.cause = Cause::Settled;
        let seq = l.append(settled, 5_000).unwrap();
        assert_eq!(l.get(seq).unwrap().supersedes, None);
        assert_eq!(l.losses().orphaned_revisions, 1);
    }

    #[test]
    fn for_track_lists_records_in_seq_order() {
        let mut l = log(ObservationPolicy::records());
        l.append(draft(1, 0, "K5ARH", Reason::Repetition), 0);
        l.append(draft(2, 0, "W1AW", Reason::Repetition), 1);
        l.append(draft(1, 3, "K5ARH", Reason::Dedupe), 2);
        let seqs: Vec<u64> = l.for_track(1).map(|o| o.seq).collect();
        assert_eq!(seqs, vec![0, 2]);
    }

    #[test]
    fn take_appended_returns_each_record_once() {
        let mut l = log(ObservationPolicy::records());
        l.append(draft(1, 0, "K5ARH", Reason::Repetition), 0);
        l.append(draft(1, 1, "K5ARH", Reason::Repetition), 1);
        let first: Vec<u64> = l.take_appended().iter().map(|o| o.seq).collect();
        assert_eq!(first, vec![0, 1]);
        assert!(l.take_appended().is_empty());
        l.append(draft(1, 2, "K5ARH", Reason::Repetition), 2);
        let second: Vec<u64> = l.take_appended().iter().map(|o| o.seq).collect();
        assert_eq!(second, vec![2]);
        assert_eq!(l.losses().unsent, 0);
    }

    #[test]
    fn a_record_expired_before_take_appended_counts_as_unsent() {
        let policy = ObservationPolicy {
            max_records: 1,
            ..ObservationPolicy::records()
        };
        let mut l = log(policy);
        l.append(draft(1, 0, "K5ARH", Reason::Repetition), 0);
        l.append(draft(1, 1, "K5ARH", Reason::Repetition), 1);
        assert_eq!(l.losses().unsent, 1);
        let taken: Vec<u64> = l.take_appended().iter().map(|o| o.seq).collect();
        assert_eq!(taken, vec![1]);
    }

    #[test]
    fn note_discontinuity_advances_the_epoch() {
        let mut l = log(ObservationPolicy::records());
        assert_eq!(l.epoch(), 0);
        l.note_discontinuity();
        l.note_discontinuity();
        assert_eq!(l.epoch(), 2);
    }

    #[test]
    fn a_trace_longer_than_the_cap_is_truncated() {
        let text = "A".repeat(200);
        let trace = TextTrace::new(&text, &[0.5; 200], &[]);
        assert_eq!(trace.text.len(), MAX_TRACE_CHARS);
        assert_eq!(trace.confidences.len(), MAX_TRACE_CHARS);
        assert!(trace.truncated);
        let short = TextTrace::new("K5?RH", &[0.9, 0.9, 0.0, 0.9, 0.9], &[]);
        assert!(!short.truncated);
        assert_eq!(short.erasures, vec![2]);
        assert!(short.alternatives.is_empty());
    }

    #[test]
    fn records_detail_drops_a_supplied_trace() {
        let mut l = log(ObservationPolicy::records());
        let mut d = draft(1, 0, "K5ARH", Reason::Repetition);
        d.trace = Some(TextTrace::new("K5ARH", &[0.9; 5], &[]));
        l.append(d, 0);
        assert_eq!(l.get(0).unwrap().draft.trace, None);
    }

    #[test]
    fn reason_all_is_in_declaration_order() {
        for (i, r) in Reason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i, "{r:?}");
        }
    }

    #[test]
    fn observation_id_displays_as_track_and_word() {
        assert_eq!(id(1, 2).to_string(), "t1.w2");
    }

    #[test]
    fn counts_cover_every_verdict_reason_pair() {
        let mut l = log(ObservationPolicy::summary());
        l.count_decision(None);
        for r in Reason::ALL {
            l.count_decision(Some(r));
        }
        let pairs: Vec<(Verdict, Option<Reason>, u64)> = l.counts().iter().collect();
        assert_eq!(pairs.len(), 1 + Reason::ALL.len());
        assert_eq!(pairs[0], (Verdict::Spotted, None, 1));
        assert!(pairs.iter().all(|&(_, _, n)| n == 1));
        assert!(pairs.contains(&(Verdict::Deferred, Some(Reason::BeaconPending), 1)));
        assert!(pairs.contains(&(Verdict::Deferred, Some(Reason::LookupPending), 1)));
        assert_eq!(
            l.counts().decided(Verdict::Spotted, Some(Reason::Dedupe)),
            0
        );
        assert_eq!(l.counts().total(), 1 + Reason::ALL.len() as u64);
    }
}
