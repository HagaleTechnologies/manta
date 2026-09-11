//! Repetition gate: a callsign must decode >= 2 distinct times within a
//! 90 s window on its track before first spot. SPEC §4.6 / ARCHITECTURE
//! §6.4. `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule
//! 2). `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly
//! into whether/when a `Spot` is emitted.
//!
//! MAN-100 Scenario 2: "distinct decodes" means distinct *messages*, not
//! distinct decoded `Word`s. SPEC's own default payload template repeats
//! a callsign back-to-back within one transmission ("CQ CQ DE <CALL>
//! <CALL> K"), which the old word-count-only accounting let satisfy this
//! gate on the strength of a single, possibly-corrupted message alone --
//! see `MIN_MESSAGE_WORD_GAP`.

use std::collections::BTreeMap;

/// Shared with `support::SupportLedger` (MAN-100 remediation C6) so the
/// gate and the ledger can never drift into arbitrating against a
/// different window than the one the repetition gate itself uses.
pub(crate) const WINDOW_SECONDS: f64 = 90.0;

/// MAN-100 Scenario 2. SPEC's payload template "CQ CQ DE <CALL> <CALL> K"
/// puts one message's two utterances a single word apart; the closest two
/// *separate* messages can put them is five ("<CALL> K CQ CQ DE
/// <CALL>"). 3 sits between the two with margin. Measured (this ticket's
/// plan): gap = 2 and gap = 3 give identical outcomes on the reference
/// pileup fixtures; 3 is the stricter reading of "a second, later message
/// must independently support the candidate", so 3 is what ships.
pub const MIN_MESSAGE_WORD_GAP: u64 = 3;

/// Width, in seconds of `sample_ts` (never real wall clock), beyond which
/// two occurrences of the same text cannot plausibly belong to one
/// transmission, regardless of their word_seq gap (MAN-100 remediation
/// C2). A short ID -- e.g. "DE <CALL>", 2 words -- puts genuinely
/// separate messages below `MIN_MESSAGE_WORD_GAP`, and a pure word_seq
/// rule then has no way to tell them apart from one corrupted message's
/// double utterance (measured: "DE K5ARH" repeated 10x at 80s spacing,
/// 13 minutes total, never spotted at all under a word_seq-only rule).
/// 60s comfortably covers a full "CQ CQ DE <CALL> <CALL> K" transmission
/// even at 8 WPM (SPEC-decode-core.md's slowest supported speed, ~40s for
/// that template) with margin, while staying well under the 80s spacing
/// that must count as separate and under the 90s ledger/gate window
/// itself.
pub const MIN_MESSAGE_TIME_GAP_SECONDS: f64 = 60.0;

/// The indices into `occurrences` (word_seq, sample_ts, track_id) triples --
/// assumed ascending in `sample_ts` (and, within any one run of matching
/// `track_id`s, in `word_seq` too, since `word_seq` only resets at a track
/// boundary) -- that count toward
/// message-distinctness: an occurrence counts if it's the first, if it
/// clears `MIN_MESSAGE_WORD_GAP` word_seqs *or* `time_gap_samples`
/// sample_ts beyond the previously *counted* occurrence (same track_id
/// only -- `word_seq` isn't comparable across a differing `track_id`), or
/// if its `track_id` differs from the previously *counted* occurrence's
/// AND `is_track_active` reports that PREVIOUS occurrence's track_id as no
/// longer active (or the full `time_gap_samples` has passed regardless).
///
/// **Only safe to call with an `is_track_active` whose answer for a given
/// track_id never changes across calls** (Codex review, PR #133 round 6):
/// this function is a pure, stateless recompute over the WHOLE slice every
/// time, so calling it again later with a DIFFERENT liveness reading for
/// the same historical track_id changes that pair's verdict retroactively
/// -- correct the first time (evaluated in real time, as the occurrence
/// arrives), wrong on replay (a track's later close shouldn't reclassify
/// an already-settled historical comparison). `support::SupportLedger` is
/// safe: its entries are keyed by `(track_id, text)`, so every occurrence
/// in one call shares one `track_id` and the cross-track branch is
/// structurally unreachable -- `is_track_active` is passed as `&|_|
/// false` there and never actually queried. `RepetitionGate`, whose
/// entries span multiple track_ids and whose liveness answers genuinely
/// drift as tracks close over the life of the gate, does NOT use this
/// function -- see `classify_new_occurrence` and `GateEntry::accepted`'s
/// own doc for why it needs a different, incremental design instead.
///
/// The track_id branch exists because `word_seq` is a per-track word index
/// (it resets to 0 on every new track), so it is not comparable across two
/// different tracks at all -- without SOME cross-track allowance, a real
/// signal's confirming re-decode on a fresh `track_id` after a close+reopen
/// (MAN-166) could land on a `word_seq`/`sample_ts` pair close enough to the
/// prior track's own that it got collapsed into "the same message" and
/// never reached the repetition floor.
///
/// A differing `track_id` is NOT by itself sufficient, nor is clearing a
/// short elapsed-time threshold (Codex review, PR #133, two rounds): a
/// *genuine* "CQ CQ DE `<CALL>` `<CALL>` K" transmission's own two call
/// utterances can legitimately be several seconds apart at ordinary CW
/// speeds -- comfortably past any elapsed-time threshold short enough to
/// still recognize a fast real track reopen, yet still one message. What
/// actually distinguishes a genuine reopen (MAN-166: track A closes, THEN
/// track B opens and repeats) from two duplicate-spawn tracks splitting one
/// transmission (track A and track B are *simultaneously* open, each
/// catching a different word of the same doubled call) is whether the
/// PRIOR track has actually closed by the time the new one's occurrence
/// arrives -- not how much `sample_ts` separates them. `is_track_active` is
/// that signal, supplied by the caller (`Validator` checks its own
/// `self.tracks` map, which `TrackClosed`'s MAN-19 teardown keeps
/// authoritative). The `time_gap_samples` OR-clause is kept as a fallback
/// for a caller that can't or doesn't track liveness (e.g. a saved-report
/// replay tool) -- at 60s it's comfortably past any real transmission, so
/// it can never itself manufacture a false confirmation the way a 1s
/// threshold could. Shared logic (the same word_seq/time_gap/track_id
/// decision rule) with `classify_new_occurrence` below, which `Repetition
/// Gate` uses instead -- `support::SupportLedger::support_in_window` folds
/// `conf_sum` over exactly the occurrences this returns.
pub(crate) fn message_distinct_indices(
    occurrences: &[(u64, u64, u32)],
    time_gap_samples: u64,
    is_track_active: &impl Fn(u32) -> bool,
) -> Vec<usize> {
    let mut counted = Vec::new();
    let mut last_counted: Option<(u64, u64, u32)> = None;
    for (i, &(seq, ts, tid)) in occurrences.iter().enumerate() {
        let counts = match last_counted {
            None => true,
            Some((prev_seq, prev_ts, prev_tid)) => {
                let gap = ts.saturating_sub(prev_ts);
                if tid != prev_tid {
                    !is_track_active(prev_tid) || gap >= time_gap_samples
                } else {
                    seq >= prev_seq + MIN_MESSAGE_WORD_GAP || gap >= time_gap_samples
                }
            }
        };
        if counts {
            counted.push(i);
            last_counted = Some((seq, ts, tid));
        }
    }
    counted
}

/// The incremental counterpart to `message_distinct_indices`, for
/// `RepetitionGate` (Codex review, PR #133 round 6). Classifies exactly
/// ONE new occurrence -- decided once, right now, against `accepted`'s
/// last entry that was itself classified message-distinct (searching
/// backward, skipping any that weren't) -- and that verdict is then
/// frozen forever in the entry pushed to `accepted`. Never re-run the
/// whole-history version of this decision against `accepted` later: doing
/// so would re-read `is_track_active` for a track_id whose liveness may
/// have changed since the pair was first (correctly) compared in real
/// time, retroactively promoting an already-settled historical pair --
/// e.g. two overlapping tracks A and B, B's occurrence correctly collapsed
/// into A's while A was still active, then A closes; a later recompute
/// would see A "inactive" and wrongly count B's already-decided
/// occurrence as a second message from what was really one transmission.
/// Same decision rule as `message_distinct_indices`, applied once instead
/// of replayed.
fn classify_new_occurrence(
    accepted: &[(u64, u64, u32, bool)],
    seq: u64,
    ts: u64,
    tid: u32,
    time_gap_samples: u64,
    is_track_active: &impl Fn(u32) -> bool,
) -> bool {
    let last_counted = accepted.iter().rev().find(|&&(.., counted)| counted);
    match last_counted {
        None => true,
        Some(&(prev_ts, prev_seq, prev_tid, _)) => {
            let gap = ts.saturating_sub(prev_ts);
            if tid != prev_tid {
                !is_track_active(prev_tid) || gap >= time_gap_samples
            } else {
                seq >= prev_seq + MIN_MESSAGE_WORD_GAP || gap >= time_gap_samples
            }
        }
    }
}

/// Width of a frequency bucket, in Hz (MAN-166). See `RepetitionGate`'s
/// doc for why bucketing exists at all, and `record`'s doc for why a
/// bucket alone isn't sufficient identity.
const FREQ_BUCKET_HZ: f64 = 100.0;

/// Minimum gap, in seconds, between two *different tracks'* decodes for
/// the later one to count as a genuinely distinct occurrence rather than
/// a near-duplicate (Codex review, PR #152). Bucketing by frequency alone
/// (see `record`'s doc) can't tell a duplicate-spawn track decoding the
/// SAME over-the-air transmission as another track -- a known real
/// phenomenon (spectral splatter spawning more than one candidate for one
/// signal) that `merge_converged` doesn't always catch before both reach
/// a full decode -- from a genuine, later re-transmission. Two duplicate
/// tracks' `WordBoundary`s land within roughly one word's keying duration
/// of each other; a real re-transmission (the operator re-keying) is a
/// real pause later. 1.0s sits below any realistic contest CQ repeat
/// cadence while comfortably exceeding a single word's decode duration --
/// a reasoned choice, not yet measured against real data the way
/// `on_snr_db`/`CHAR_GAP_DITS` were. Only applies across *different*
/// track_ids: the same track decoding two words close together (a real
/// pattern -- a CQing station double-calling its own callsign
/// back-to-back within one transmission, e.g. "CQ K5ARH K5ARH K",
/// specifically so the transmission carries its own two confirmations)
/// is always genuine, since one continuous decode stream can't decode the
/// same instant twice.
pub(crate) const MIN_OCCURRENCE_GAP_SECONDS: f64 = 1.0;

/// Clamps well inside `i64`'s range so `record`'s `b - 1..=b + 1` neighbor
/// arithmetic can never overflow (Codex review, PR #152) -- a non-finite
/// or astronomically large `freq_hz` (e.g. a corrupted `center_freq_hz` in
/// a WAV sidecar, `crates/manta-input/src/lib.rs`) would otherwise let the
/// cast saturate to `i64::MAX`/`MIN`, and `b + 1`/`b - 1` on that panics in
/// debug builds or wraps in release. No real RF frequency comes anywhere
/// close to this bound.
const SAFE_BUCKET_BOUND: f64 = (i64::MAX / 2) as f64;

fn bucket(freq_hz: f64) -> i64 {
    if !freq_hz.is_finite() {
        return 0;
    }
    (freq_hz / FREQ_BUCKET_HZ)
        .clamp(-SAFE_BUCKET_BOUND, SAFE_BUCKET_BOUND)
        .round() as i64
}

#[derive(Default)]
struct GateEntry {
    /// `(sample_ts, word_seq, track_id, message_distinct)` of every
    /// *accepted* (distinct, non-near-duplicate) occurrence. The returned
    /// repetition count is not simply this vec's length: MAN-100 Scenario 2
    /// requires accepted occurrences to also be message-distinct, since
    /// SPEC's own default payload template repeats a callsign back-to-back
    /// within one transmission and both utterances land here as separate
    /// *accepted* occurrences (they're minutes, not
    /// `MIN_OCCURRENCE_GAP_SECONDS`, apart) despite being one message's
    /// worth of evidence.
    ///
    /// `message_distinct` is decided ONCE, by `classify_new_occurrence`,
    /// at the moment this occurrence is pushed -- and never revisited
    /// afterward (Codex review, PR #133 round 6): earlier revisions
    /// recomputed this flag fresh across the WHOLE vector on every
    /// `record` call, which let a track's liveness changing later (e.g.
    /// closing) retroactively flip an already-settled historical pair's
    /// verdict, manufacturing a false confirmation from what both times
    /// was really one transmission. See `classify_new_occurrence`'s doc.
    accepted: Vec<(u64, u64, u32, bool)>,
    /// Every track_id that has touched this entry -- accepted *or*
    /// rejected as a near-duplicate -- and when it was last seen (Codex
    /// review, PR #152, round 6): without this, a track whose first
    /// decode was rejected as a near-duplicate of a *different* track's
    /// could never establish its own identity here, so its own later,
    /// genuinely distinct repeat would keep being compared against the
    /// other track's timestamp instead of recognizing itself.
    last_seen_by_track: BTreeMap<u32, u64>,
}

impl GateEntry {
    /// Most recent activity of any kind (accepted or merely seen),
    /// `None` if this entry has nothing at all -- used to pick the
    /// *freshest* eligible neighbor bucket in `record` (Codex review, PR
    /// #152) rather than always the lowest-numbered one, which could
    /// otherwise select a not-yet-swept but effectively-expired entry
    /// (sweeps are throttled to a periodic interval, not run on every
    /// call) ahead of a genuinely fresher one a different neighbor bucket
    /// away.
    ///
    /// Uses `accepted`'s actual maximum, not `.last()` (Codex review, PR
    /// #152, round 13): `Validator::resolve_pending_beacons` can replay a
    /// deferred candidate's older `sample_ts` well after a newer one
    /// already landed on the same entry (a long-lived track's Beacon
    /// candidate is only judged at track close, which can happen long
    /// after other, more recent activity already touched this entry
    /// through the ordinary path) -- `accepted` is not guaranteed to stay
    /// in timestamp order, so `.last()` (the most-recently-*pushed*
    /// element) can be older than the true latest occurrence, wrongly
    /// reporting this entry as stale and letting `record`'s expired-home
    /// check delete still-live repetition credit.
    fn most_recent(&self) -> Option<u64> {
        self.accepted
            .iter()
            .map(|&(ts, _, _, _)| ts)
            .max()
            .into_iter()
            .chain(self.last_seen_by_track.values().copied())
            .max()
    }
}

pub struct RepetitionGate {
    window_samples: u64,
    min_occurrence_gap_samples: u64,
    /// MAN-100 remediation C2: `MIN_MESSAGE_TIME_GAP_SECONDS` in samples.
    time_gap_samples: u64,
    /// Keyed by a frequency bucket (not `track_id`, MAN-166): a real
    /// signal's `track_id` changes every time its track closes and
    /// reopens (e.g. `CloseReason::HangExpired`'s 5s silence timer), so
    /// keying repetition memory by `track_id` defeated this gate's own
    /// 90s window the instant a track churned, regardless of whether the
    /// same callsign was still genuinely repeating. A frequency bucket
    /// survives that churn.
    seen: BTreeMap<(i64, String), GateEntry>,
    /// Cumulative count of `record()` calls, for life. MAN-19 round 3:
    /// the only direct evidence that this gate's state was ever touched
    /// at all -- a soak whose decoding regressed to metadata-only output
    /// (TrackMeta/SpeedUpdate, no CharDecoded ever reaching a candidate
    /// word) could still open/close tracks and pass every other
    /// workload-activity check while never calling `record`, leaving
    /// `sweep`'s half of the leak fix completely unexercised.
    records_total: u64,
}

impl RepetitionGate {
    pub fn new(fs: f64) -> Self {
        Self {
            window_samples: (WINDOW_SECONDS * fs) as u64,
            min_occurrence_gap_samples: (MIN_OCCURRENCE_GAP_SECONDS * fs) as u64,
            time_gap_samples: (MIN_MESSAGE_TIME_GAP_SECONDS * fs) as u64,
            seen: BTreeMap::new(),
            records_total: 0,
        }
    }

    /// Records one decode of `callsign` by `track_id` at `freq_hz` at
    /// `sample_ts`, originating from the track's `word_seq`-numbered
    /// `Word` (MAN-100 Scenario 2). Returns the number of
    /// *message*-distinct decodes within the trailing window (including
    /// this one) -- two accepted occurrences fewer than
    /// `MIN_MESSAGE_WORD_GAP` words apart *and* less than
    /// `MIN_MESSAGE_TIME_GAP_SECONDS` apart in sample_ts count as one,
    /// since SPEC's own default payload template repeats a callsign
    /// back-to-back within a single transmission.
    ///
    /// A single `bucket(freq_hz)` lookup isn't sufficient identity on its
    /// own: two decodes of the same real signal can round to *different*
    /// buckets if the centroid happens to drift across a bucket boundary
    /// between them (Codex review, PR #152 -- e.g. 14,000,049 Hz then
    /// 14,000,051 Hz round to buckets 140000 and 140001 despite being 2 Hz
    /// apart). So a new decode whose own home bucket has never been
    /// touched checks both neighbors for an existing entry under the same
    /// callsign, and *moves* (renames) that one in if found, rather than
    /// always keying strictly by its own freshly-computed bucket -- this
    /// keeps a drifting signal's anchor following it forward across
    /// successive hops, each individually within the +-1 tolerance, so a
    /// later hop's own neighbor search can still find it. But a home
    /// bucket that already HAS an entry always uses that entry directly,
    /// never a neighbor's, however fresh (Codex review, PR #152, round 9):
    /// two adjacent buckets can each independently hold a genuinely
    /// distinct real signal's own history, and letting a fresher neighbor
    /// outrank an already-established home identity could import that
    /// DIFFERENT signal's history into this one -- worse, chaining that
    /// across successive calls could transitively combine two entries
    /// that were never actually the same signal. A neighbor is only ever
    /// moved into an empty home, never merged into one that already holds
    /// its own history.
    ///
    /// That widened net cuts the other way too (same review): two
    /// genuinely different, simultaneous tracks decoding the same real
    /// transmission (a known duplicate-candidate phenomenon
    /// `merge_converged` doesn't always catch in time) could land in the
    /// same bucket -- the exact one, not just a neighbor -- and both
    /// credit the same entry within milliseconds of each other. `track_id`
    /// is what actually distinguishes that from a real pattern (a CQing
    /// station double-calling its own callsign back-to-back within one
    /// transmission): see `MIN_OCCURRENCE_GAP_SECONDS`'s doc. A track's
    /// own first decode, even if rejected as a near-duplicate of a
    /// different track's, must not block that same track's own later,
    /// genuinely distinct repeat -- but that exemption only applies when
    /// the track's own *previous* touch was itself recent (Codex review,
    /// PR #152, round 8: bare historical presence, no matter how old,
    /// must not grant a blanket bypass -- a long-lived duplicate-spawn
    /// track touching this entry once, long ago, and then touching it
    /// again right next to a DIFFERENT track's brand-new decode must
    /// still be checked against that other track's near-simultaneous
    /// activity, or an old rejected identity can manufacture a second
    /// confirmation for what both times was really the same one
    /// over-the-air occurrence).
    ///
    /// A non-finite `freq_hz` (NaN/±infinity -- should never happen from
    /// the real DSP pipeline, but a defensive guard, Codex review PR
    /// #152) is rejected outright: no entry is touched, and this returns
    /// 0 (never `>= 2`, so it can never itself satisfy the repetition
    /// gate). Silently mapping it to some fallback bucket instead would
    /// risk cross-contaminating repetition credit with whatever a real
    /// signal happens to already occupy there -- bucket 0 in particular
    /// is already a live sentinel elsewhere in this codebase for "unknown
    /// center frequency" (`WavIqSource`'s missing-sidecar default).
    pub fn record(
        &mut self,
        track_id: u32,
        freq_hz: f64,
        callsign: &str,
        sample_ts: u64,
        word_seq: u64,
        is_track_active: impl Fn(u32) -> bool,
    ) -> usize {
        self.records_total += 1;
        if !freq_hz.is_finite() {
            return 0;
        }
        let b = bucket(freq_hz);
        let home_key = (b, callsign.to_string());
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        // Codex review, PR #152, round 10: a home entry that's aged past
        // the trailing window but hasn't been swept yet (sweeps are
        // throttled, not run on every call) must not block a genuinely
        // fresh neighbor -- discard it before the "prefer home" rule
        // below gets a chance to pin a decode to dead history.
        if let Some(existing) = self.seen.get(&home_key) {
            let home_expired = existing.most_recent().is_none_or(|ts| ts < cutoff);
            if home_expired {
                self.seen.remove(&home_key);
            }
        }
        // Codex review, PR #152, round 9: this call's own home bucket, if
        // it already has an entry, is ALWAYS used directly -- never
        // superseded by a neighbor, however fresh. Two adjacent buckets
        // can each independently hold a genuinely distinct real signal's
        // own history; letting a fresher neighbor outrank an already-
        // established home identity (round 7/8's behavior) could import
        // that DIFFERENT signal's history into this one, and worse,
        // chaining that across successive calls could transitively merge
        // two entries that were never actually the same signal at all.
        // Neighbor search is strictly a fallback for when home has NEVER
        // been touched (or was just discarded as expired above) -- the
        // genuine boundary-drift case -- and even then it only ever
        // *moves* (renames) a neighbor's entry into an empty home, never
        // merges two already-populated entries together.
        if !self.seen.contains_key(&home_key) {
            // Among both neighbors, join whichever existing entry is
            // *freshest* (see `GateEntry::most_recent`), not just the
            // lowest-numbered one that happens to still exist -- sweeps
            // are throttled, so a stale-but-not-yet-swept neighbor must
            // not be preferred over a genuinely fresher one.
            let matched_key = (b - 1..=b + 1)
                .filter(|&candidate| candidate != b)
                .map(|candidate| (candidate, callsign.to_string()))
                .filter_map(|k| {
                    let ts = self.seen.get(&k)?.most_recent()?;
                    Some((k, ts))
                })
                .max_by_key(|(_, ts)| *ts)
                .map(|(k, _)| k);
            if let Some(found_key) = matched_key {
                let moved = self.seen.remove(&found_key).unwrap();
                self.seen.insert(home_key.clone(), moved);
            }
        }
        let entry = self.seen.entry(home_key).or_default();
        // Codex review, PR #152, round 8: a track's own previous touch
        // only exempts a NEW touch from the cross-track near-duplicate
        // check when that previous touch is itself recent -- a genuine
        // rapid double-call, per this gate's own documented intent ("well
        // under a second apart"). Bare historical presence in
        // `last_seen_by_track`, no matter how old, must NOT grant a
        // blanket bypass: a long-lived duplicate-spawn track that touched
        // this entry once, long ago, and then touches it again right next
        // to a DIFFERENT track's brand-new decode must still be checked
        // against that other track's near-simultaneous activity -- an old
        // rejected identity must not be able to manufacture a second
        // confirmation for what both times was really the same one
        // over-the-air occurrence.
        let is_rapid_own_repeat = entry
            .last_seen_by_track
            .get(&track_id)
            .is_some_and(|&last| sample_ts.saturating_sub(last) < self.min_occurrence_gap_samples);
        // Codex review, PR #152, round 10: the cross-track gap check
        // compares against the entry's MOST RECENT activity overall
        // (`GateEntry::most_recent` -- accepted or merely seen), not just
        // the last *accepted* timestamp. With three or more duplicate
        // tracks staggered just under the gap threshold from each other
        // (A accepted at 0, B rejected at 0.9s, C at 1.1s), comparing only
        // against A's accepted timestamp lets C clear the gap (1.1s) even
        // though C is only 0.2s after B's rejected touch -- still very
        // likely the same over-the-air occurrence. Using the latest touch
        // from ANY track keeps the exclusion window extending as long as
        // near-duplicate touches keep arriving, closing that gap.
        let is_distinct_occurrence = if is_rapid_own_repeat {
            true
        } else {
            match entry.most_recent() {
                Some(last) => sample_ts.saturating_sub(last) >= self.min_occurrence_gap_samples,
                None => true,
            }
        };
        // Codex review, PR #152, round 13: never regress a track's own
        // watermark. `Validator::resolve_pending_beacons` can call
        // `record` with an older, deferred `sample_ts` well after a newer
        // one already landed for the same track_id on this entry --
        // overwriting unconditionally would move that track's own
        // last-seen time backward, corrupting `is_rapid_own_repeat`'s
        // read on any later call.
        entry
            .last_seen_by_track
            .entry(track_id)
            .and_modify(|existing| *existing = (*existing).max(sample_ts))
            .or_insert(sample_ts);
        if is_distinct_occurrence {
            // MAN-100 Scenario 2 (remediation C2, extended for cross-track
            // pairs, Codex review PR #133 round 6): classified ONCE, right
            // now, against whatever is currently `accepted`'s last
            // message-distinct entry -- see `classify_new_occurrence`'s
            // doc for why this must never be re-derived later using a
            // track's then-current (possibly since-changed) liveness.
            let message_distinct = classify_new_occurrence(
                &entry.accepted,
                word_seq,
                sample_ts,
                track_id,
                self.time_gap_samples,
                &is_track_active,
            );
            entry
                .accepted
                .push((sample_ts, word_seq, track_id, message_distinct));
        }
        entry.accepted.retain(|&(ts, _, _, _)| ts >= cutoff);
        entry.last_seen_by_track.retain(|_, ts| *ts >= cutoff);
        // Tally the frozen per-occurrence verdicts still inside the
        // window -- never re-derive them (see `accepted`'s own doc).
        entry
            .accepted
            .iter()
            .filter(|&&(.., counted)| counted)
            .count()
    }

    /// See `records_total`'s doc.
    pub fn records_total(&self) -> u64 {
        self.records_total
    }

    /// Count of distinct (frequency bucket, callsign) entries currently
    /// held, for tests and future observability (ARCHITECTURE §8) -- the
    /// direct way to confirm `sweep` is actually keeping this bounded
    /// under sustained real-world churn, as opposed to `records_total`
    /// (which only ever grows).
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// See `len`.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Prunes every entry down to its timestamps within the trailing 90s
    /// window as of `now_ts`, dropping any entry left empty. MAN-166:
    /// this is the gate's *only* forgetting mechanism -- purely
    /// time-based, matching the 90s window's own intent, rather than tied
    /// to a track's ephemeral lifecycle (that was the MAN-19 fix's bug:
    /// forgetting on every `TrackClosed` discarded genuinely-live
    /// repetition memory the instant a real signal's track churned).
    /// Still bounds memory the way MAN-19 needed: without a periodic
    /// sweep, an entry whose bucket+callsign is never recorded again
    /// would sit in `seen` forever, since `record`'s own `retain` only
    /// prunes an entry's timestamps when that same key is recorded again.
    /// Call periodically -- `Validator::ingest` calls this once per
    /// `TrackClosed`, which fires often enough under real track churn to
    /// keep `seen` bounded without needing a dedicated timer.
    pub fn sweep(&mut self, now_ts: u64) {
        let cutoff = now_ts.saturating_sub(self.window_samples);
        self.seen.retain(|_, entry| {
            entry.accepted.retain(|&(ts, _, _, _)| ts >= cutoff);
            entry.last_seen_by_track.retain(|_, ts| *ts >= cutoff);
            !entry.accepted.is_empty() || !entry.last_seen_by_track.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    /// Every test below that doesn't care whether some OTHER track_id is
    /// still "active" (either it's single-track throughout, or the
    /// cross-track pair it exercises is already decided by something else,
    /// e.g. the near-duplicate-time rejection) passes this.
    fn always_active(_: u32) -> bool {
        true
    }

    #[test]
    fn first_decode_counts_as_one() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active);
        assert_eq!(
            gate.record(1, 7_080_000.0, "K5ARH", 300_000, 10, always_active),
            2
        );
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(
            gate.record(
                1,
                7_080_000.0,
                "K5ARH",
                window_samples + 1,
                10,
                always_active
            ),
            1
        );
    }

    #[test]
    fn different_frequencies_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active);
        // Far enough apart (>1 bucket width) to land outside neighbor matching.
        assert_eq!(gate.record(1, 7_081_000.0, "K5ARH", 0, 0, always_active), 1);
        assert_eq!(gate.record(1, 7_080_000.0, "W1AW", 0, 0, always_active), 1);
    }

    /// MAN-100 Scenario 2: two occurrences fewer than `MIN_MESSAGE_WORD_GAP`
    /// words apart -- e.g. the two adjacent utterances in one "CQ CQ DE
    /// <CALL> <CALL> K" transmission -- are one message's worth of
    /// evidence, not two.
    #[test]
    fn adjacent_words_are_one_message_not_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 4, always_active);
        assert_eq!(
            gate.record(1, 7_080_000.0, "K5ARH", 10_000, 5, always_active),
            1,
            "seq 4 and 5 are one message"
        );
    }

    /// MAN-100 Scenario 2: the flip side -- occurrences at least
    /// `MIN_MESSAGE_WORD_GAP` words apart are genuinely separate messages
    /// and both count.
    #[test]
    fn words_three_apart_are_two_messages() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 4, always_active);
        assert_eq!(
            gate.record(1, 7_080_000.0, "K5ARH", 100_000, 7, always_active),
            2
        );
    }

    /// MAN-100 remediation C2: a short "DE <CALL>" ID puts the callsign
    /// only 2 words apart even across genuinely separate transmissions --
    /// below `MIN_MESSAGE_WORD_GAP`. The time-based OR clears it instead:
    /// 80s of sample_ts is well past `MIN_MESSAGE_TIME_GAP_SECONDS`, so
    /// this must still count as two messages despite the short word gap.
    #[test]
    fn a_short_id_repeated_with_a_wide_time_gap_counts_as_two_messages() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 4, always_active);
        let eighty_seconds_samples = (80.0 * FS) as u64;
        assert_eq!(
            gate.record(
                1,
                7_080_000.0,
                "K5ARH",
                eighty_seconds_samples,
                6,
                always_active
            ),
            2,
            "word gap is only 2, but 80s of sample_ts must still separate \
             two genuinely distinct transmissions of a short ID"
        );
    }

    /// The flip side of the above: a short word gap AND a short time gap
    /// together still mean one message -- the time-based OR must not fire
    /// spuriously on ordinary adjacent-word repeats.
    #[test]
    fn a_short_word_gap_and_a_short_time_gap_together_still_count_as_one_message() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 4, always_active);
        assert_eq!(
            gate.record(1, 7_080_000.0, "K5ARH", 10_000, 5, always_active),
            1,
            "10 000 samples (~0.1s) is nowhere near \
             MIN_MESSAGE_TIME_GAP_SECONDS, so the word-gap rule alone \
             should decide, same as before this change"
        );
    }

    /// Codex review, PR #133 (P1, both rounds): a differing `track_id`
    /// alone must NOT be sufficient for message-distinctness -- what
    /// actually distinguishes a genuine track reopen from two duplicate-
    /// spawn tracks splitting one transmission is whether the PRIOR track
    /// has genuinely closed, not how much `sample_ts` separates them (round
    /// 2 found round 1's elapsed-time-only fix still insufficient: a real
    /// transmission's own two call utterances can legitimately be several
    /// seconds apart). Reproduces the exact failure mode: two duplicate-
    /// spawn tracks (A, B), BOTH still active, decoding one doubled-call
    /// transmission ("CQ CQ DE K5ARH K5ARH K") near-simultaneously. Track
    /// A's first copy is accepted; track B's own near-simultaneous first
    /// copy is rejected as a near-duplicate; track B's adjacent SECOND
    /// copy is then accepted anyway via its own rapid-own-repeat
    /// exemption, landing a second, different-track_id, still-close-in-
    /// time occurrence in `accepted`. Since track A is still active
    /// (`always_active`), this must not count as a second message.
    #[test]
    fn a_duplicate_spawn_tracks_second_copy_does_not_manufacture_a_second_message() {
        let mut gate = RepetitionGate::new(FS);
        // Track 1's first copy of the doubled call.
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 0, 1, always_active), 1);
        // Track 2 (duplicate spawn of the same over-the-air signal) decodes
        // its OWN first copy 0.3s later -- within MIN_OCCURRENCE_GAP_SECONDS
        // of track 1's touch, so rejected as a near-duplicate (still 1).
        assert_eq!(
            gate.record(2, 7_080_000.0, "K5ARH", 30_000, 1, always_active),
            1
        );
        // Track 2's adjacent second copy, 0.3s after ITS OWN first touch --
        // accepted via its own rapid-own-repeat exemption. Track 1 is
        // still active (this is the whole point of the duplicate-spawn
        // scenario: both tracks alive at once), so this must still read 1,
        // not 2.
        assert_eq!(
            gate.record(2, 7_080_000.0, "K5ARH", 60_000, 2, always_active),
            1,
            "track 2's own doubled utterance of the SAME transmission must \
             not manufacture a false second confirmation just because it \
             landed on a different track_id, while track 1 is still active"
        );
    }

    /// Codex review, PR #133 (P1, round 2): the deeper failure round 1's
    /// fix missed. Two duplicate-spawn tracks (A, B), BOTH still active the
    /// whole time, split the TWO call utterances of one doubled-call
    /// transmission between them -- A catches the first "K5ARH", B catches
    /// the second, ~3.4s later (a plausible real word-plus-gap duration at
    /// ordinary CW speed, comfortably past `MIN_OCCURRENCE_GAP_SECONDS` but
    /// nowhere near `MIN_MESSAGE_TIME_GAP_SECONDS`). A round-1 fix keyed
    /// purely on elapsed time would have counted this as 2 genuine
    /// messages; since track A never actually closes, it must still read 1.
    #[test]
    fn a_transmission_split_across_two_still_active_tracks_is_one_message() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 0, 1, always_active), 1);
        let three_point_four_seconds = (3.4 * FS) as u64;
        assert_eq!(
            gate.record(
                2,
                7_080_000.0,
                "K5ARH",
                three_point_four_seconds,
                1,
                always_active
            ),
            1,
            "track A is still active, so track B's copy of the SAME \
             transmission's second utterance must not manufacture a false \
             second confirmation just because several real seconds \
             separate the two words"
        );
    }

    /// Codex review, PR #133 (round 6): a message-distinctness verdict,
    /// once decided for a specific occurrence, must never be revisited
    /// later using a track's CURRENT (possibly since-changed) liveness.
    /// Track 1 (A) is open when track 2 (B) decodes the same callsign --
    /// B's occurrence is correctly collapsed into A's message while A is
    /// still active. A later, unrelated `record` call touching the SAME
    /// entry, after A has since closed, must not retroactively flip B's
    /// already-settled verdict -- inspected directly on the frozen
    /// `accepted` entries, not just the returned count (which, taken
    /// alone, can't distinguish "B correctly stayed collapsed" from "B was
    /// wrongly promoted but something else happened to net out the same
    /// total").
    #[test]
    fn a_settled_message_distinct_verdict_is_never_revisited_after_the_fact() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active), 1);
        let three_point_four_seconds = (3.4 * FS) as u64;
        assert_eq!(
            gate.record(
                2,
                7_080_000.0,
                "K5ARH",
                three_point_four_seconds,
                0,
                always_active
            ),
            1,
            "B's occurrence must collapse into A's message while A is active"
        );

        let key = (70_800i64, "K5ARH".to_string());
        let entry = gate.seen.get(&key).expect("entry must exist");
        assert_eq!(
            entry.accepted.len(),
            2,
            "both occurrences must be accepted (non-near-duplicate)"
        );
        assert!(
            entry.accepted[0].3,
            "A's occurrence must be message-distinct"
        );
        assert!(
            !entry.accepted[1].3,
            "B's occurrence must be collapsed, not message-distinct"
        );

        // A later `record` call touching the SAME entry, comfortably past
        // the near-duplicate gap, now reporting track 1 (A) as inactive --
        // simulating A having closed in the meantime. This must NOT
        // retroactively flip B's already-settled verdict.
        let ten_seconds = (10.0 * FS) as u64;
        gate.record(3, 7_080_000.0, "K5ARH", ten_seconds, 0, |tid| tid != 1);

        let entry = gate.seen.get(&key).expect("entry must exist");
        assert!(
            entry.accepted[0].3,
            "A's occurrence must still be message-distinct"
        );
        assert!(
            !entry.accepted[1].3,
            "B's already-settled verdict must never be revisited using A's \
             NOW-changed liveness"
        );
    }

    /// A real signal's track closing and reopening under a new `track_id`
    /// (e.g. `CloseReason::HangExpired`) must not reset its repetition
    /// count -- that's the whole point of keying by frequency bucket
    /// instead of `track_id` (MAN-166). Two `record` calls for the same
    /// bucket+callsign under *different* track_ids (simulating the close
    /// and reopen -- track 1 reported no longer active), with a `sweep`
    /// between them (as `Validator::ingest` does on every `TrackClosed`),
    /// still accumulate to 2.
    #[test]
    fn sweep_between_two_records_does_not_reset_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active);
        gate.sweep(0);
        assert_eq!(
            gate.record(2, 7_080_000.0, "K5ARH", 300_000, 10, |tid| tid != 1),
            2
        );
    }

    /// `sweep` prunes only entries whose timestamps have fully aged out of
    /// the trailing 90s window as of `now_ts` -- a still-live entry must
    /// survive.
    #[test]
    fn sweep_prunes_only_entries_older_than_the_window() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;

        gate.record(1, 7_080_000.0, "K5ARH", 0, 0, always_active); // will age out, never refreshed
        gate.record(2, 7_090_000.0, "W1AW", 0, 0, always_active);
        // A genuine later occurrence (comfortably past both the
        // minimum-occurrence gap and, eventually, `now`'s cutoff) keeps
        // W1AW's entry alive.
        gate.record(
            2,
            7_090_000.0,
            "W1AW",
            window_samples + 1,
            10,
            always_active,
        );

        gate.sweep(window_samples * 2);

        assert_eq!(
            gate.len(),
            1,
            "K5ARH's only decode is older than the window and must have been pruned; W1AW's later occurrence must survive"
        );
    }

    /// Codex review, PR #152: two decodes of the same real signal 2 Hz
    /// apart (14,000,049 Hz then 14,000,051 Hz) round to *different*
    /// 100 Hz buckets (140000 vs 140001) despite being the same signal --
    /// the exact centroid-drift-across-a-boundary case this bucketing
    /// exists to tolerate. Both must still count toward the same
    /// repetition total (different track_ids, simulating the track that
    /// drifted closing -- reported inactive -- and reopening).
    #[test]
    fn a_decode_just_across_a_bucket_boundary_still_counts_toward_the_same_signal() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(
            gate.record(1, 14_000_049.0, "K5ARH", 0, 0, always_active),
            1
        );
        assert_eq!(
            gate.record(2, 14_000_051.0, "K5ARH", 300_000, 10, |tid| tid != 1),
            2
        );
    }

    /// Codex review, PR #152 (both rounds): two genuinely *different*,
    /// simultaneous tracks decoding the same real over-the-air
    /// transmission (a known real phenomenon `merge_converged` doesn't
    /// always catch in time) must not both credit the same entry --
    /// whether they land in the exact same bucket or a neighbor one. A
    /// different track_id within `MIN_OCCURRENCE_GAP_SECONDS` of the
    /// entry's latest timestamp must not count as a distinct occurrence;
    /// a genuine later re-transmission still does.
    #[test]
    fn near_simultaneous_decodes_from_a_different_track_do_not_double_count() {
        let mut gate = RepetitionGate::new(FS);
        // Same exact bucket, different (duplicate-spawn) track, same instant.
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", 1_000, 1, always_active),
            1
        );
        // Neighbor bucket, yet another track, still the same instant.
        assert_eq!(
            gate.record(3, 14_000_060.0, "K5ARH", 1_500, 2, always_active),
            1
        );
        // A real, later re-transmission on track 1's OWN track_id -- both
        // accepted occurrences share track_id 1, so message-distinctness
        // never crosses tracks here at all (tracks 2/3's touches were
        // rejected, not accepted); `always_active` is inert.
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 300_000, 10, always_active),
            2
        );
    }

    /// The minimum-gap check above must never apply *within* a single
    /// track: a real CQing station double-calling its own callsign
    /// back-to-back within one transmission ("CQ K5ARH K5ARH K", a
    /// deliberate real practice so the transmission carries its own two
    /// confirmations) decodes both instances on the same track, often
    /// well under a second apart, and must still count as two -- one
    /// continuous decode stream can't decode the same instant twice. Uses
    /// `word_seq`s >= `MIN_MESSAGE_WORD_GAP` apart (MAN-100) so this test
    /// isolates the near-duplicate-time mechanism it names, rather than
    /// colliding with the separate message-gap rule that would otherwise
    /// also collapse two genuinely adjacent words to one message.
    #[test]
    fn rapid_same_track_repeats_are_never_rejected_as_near_simultaneous() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 500, 10, always_active),
            2
        );
    }

    /// Codex review, PR #152: a non-finite `freq_hz` (NaN/±infinity) must
    /// be rejected outright -- no entry touched, `records_total` still
    /// increments (it's a real call), but the returned count is always 0
    /// and `seen` stays empty. An astronomically large but *finite*
    /// `freq_hz` (e.g. `f64::MAX`, a corrupted sidecar `center_freq_hz`)
    /// must instead never panic or wrap via `bucket`'s `± 1` neighbor
    /// arithmetic -- it's clamped to a safe bucket and recorded normally.
    #[test]
    fn non_finite_and_extreme_frequencies_do_not_panic() {
        let mut gate = RepetitionGate::new(FS);
        for freq in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(gate.record(1, freq, "K5ARH", 0, 0, always_active), 0);
        }
        assert!(
            gate.is_empty(),
            "non-finite frequencies must never create an entry"
        );
        for freq in [f64::MAX, f64::MIN] {
            gate.record(1, freq, "K5ARH", 0, 0, always_active);
        }
    }

    /// Codex review, PR #152, round 7: among the home bucket and both
    /// neighbors, `record` must join whichever existing entry is
    /// *freshest*, not just the lowest-numbered bucket that happens to
    /// still exist. Bucket `b-1` holds an old entry; bucket `b+1` holds a
    /// more recent one. A naive "first matching neighbor" search would
    /// join `b-1` (it sorts first regardless of recency) instead of the
    /// entry a real drifting signal actually continued into.
    #[test]
    fn record_prefers_the_freshest_matching_neighbor_over_the_stale_one() {
        let mut gate = RepetitionGate::new(FS);

        // Bucket b-1 (14_999_900 Hz): an old decode.
        gate.record(1, 14_999_900.0, "K5ARH", 0, 0, always_active);
        // Bucket b+1 (15_000_100 Hz): a more recent decode, different track.
        gate.record(2, 15_000_100.0, "K5ARH", 500_000, 0, always_active);

        // A new decode at the home bucket (15_000_000 Hz), on a track that
        // reports track 2 (the freshest neighbor's own toucher) as no
        // longer active -- a genuine later occurrence, not a duplicate
        // spawn -- must join the freshest neighbor (b+1) -- becoming its
        // second occurrence -- not the older, lowest-numbered one (b-1).
        assert_eq!(
            gate.record(3, 15_000_000.0, "K5ARH", 700_000, 10, |tid| tid != 2),
            2,
            "must join the freshest neighbor entry, not the stale lowest-numbered one"
        );
    }

    /// Codex review, PR #152, round 6: a track's own decode, even if its
    /// *first* appearance in an entry gets rejected as a near-duplicate of
    /// a *different* track's, must still be able to establish its own
    /// identity for a later, genuinely distinct repeat -- rejected
    /// occurrences must not be forgotten outright, or the rejected
    /// track's own next attempt gets compared against the wrong track's
    /// timestamp again and is wrongly rejected a second time.
    ///
    /// Track 1 reported inactive on the deciding call (Codex review, PR
    /// #133, round 2): message-distinctness across a `track_id` change now
    /// keys off whether the PRIOR track has genuinely closed, not elapsed
    /// time -- this test's own concern (track 2's rejected first attempt
    /// does not block its own later acceptance) is orthogonal to that, so
    /// track 1's liveness is set to whatever makes the return value
    /// defensible: closed, consistent with "track 2 decodes AGAIN" reading
    /// as a genuinely later, distinct signal rather than a duplicate spawn
    /// still racing track 1.
    #[test]
    fn a_tracks_own_repeat_counts_even_after_its_first_attempt_was_rejected() {
        let mut gate = RepetitionGate::new(FS);
        // Track 1 establishes the entry.
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        // Track 2's near-simultaneous decode is correctly rejected as a
        // likely duplicate of track 1's. `always_active` is inert here --
        // this occurrence is rejected before message-distinctness is ever
        // evaluated.
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", 500, 1, always_active),
            1
        );
        // Track 2 decodes AGAIN, shortly after its own (rejected) first
        // attempt -- this is track 2's own second word, not a duplicate
        // of anyone else, and must count as a second distinct occurrence.
        // Track 1 reported inactive: a genuine later signal, not a
        // duplicate spawn still racing it.
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", 600, 10, |tid| tid != 1),
            2
        );
    }

    /// MAN-19's original concern (unbounded growth under sustained track
    /// churn) still holds with the new key: many distinct frequency
    /// buckets, each recorded once and then aged out, must not accumulate
    /// forever once swept.
    #[test]
    fn sustained_churn_stays_bounded_once_swept_past_the_window() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        // 1 kHz apart: far outside neighbor-bucket matching range, so each
        // gets its own entry.
        for i in 0..10_000i64 {
            gate.record(i as u32, i as f64 * 1000.0, "K5ARH", 0, 0, always_active);
        }
        gate.sweep(window_samples + 1);
        assert_eq!(
            gate.seen.len(),
            0,
            "seen must not accumulate one entry per historical bucket once swept past the window"
        );
    }

    /// Codex review, PR #152, round 8: a long-lived duplicate-spawn track
    /// that touched an entry once, long ago, and was rejected then, must
    /// NOT get a blanket "always distinct" pass on a much later touch just
    /// because it has *some* historical presence. Track A accepts at t=0;
    /// track B's near-simultaneous duplicate at t=0.5s is rejected. Both
    /// tracks stay alive (no sweep ever fully clears B's identity). Track
    /// A decodes again far later (a genuine new occurrence, correctly
    /// distinct). Track B then decodes again *immediately after* -- a
    /// fresh near-duplicate of A's new occurrence, not a continuation of
    /// B's own ancient first touch -- and must still be rejected by the
    /// cross-track gap check, not waved through because B "has touched
    /// this before."
    #[test]
    fn an_old_rejected_identity_does_not_exempt_a_fresh_near_duplicate() {
        let mut gate = RepetitionGate::new(FS);
        let one_second = FS as u64;

        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        // Track B's near-simultaneous duplicate of A's first decode is
        // correctly rejected -- but B's identity is now on record.
        // `always_active` is inert: this occurrence is rejected before
        // message-distinctness is evaluated.
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", one_second / 2, 1, always_active),
            1
        );

        // Much later, but still inside the trailing 90s window: track A's
        // genuine second occurrence -- same track_id as the first, so
        // message-distinctness never crosses tracks here (B's touch was
        // never accepted); `always_active` is inert.
        let later = 89 * one_second;
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", later, 10, always_active),
            2
        );

        // Immediately after: track B decodes again. This is near-
        // simultaneous with A's fresh occurrence above, not with B's own
        // ancient first touch (88+ seconds earlier) -- must be rejected
        // as a likely duplicate of A's just-accepted occurrence, not
        // waved through as "B's own repeat." Rejected before
        // message-distinctness runs, so `always_active` is inert.
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", later + one_second / 20, 20, always_active),
            2,
            "an old rejected identity must not exempt a fresh near-duplicate of a DIFFERENT track's brand-new occurrence"
        );
    }

    /// Codex review, PR #152, round 8: a signal drifting across more than
    /// one bucket boundary over successive occurrences -- each individual
    /// hop within the +-1 neighbor tolerance -- must still be recognized
    /// as the same signal throughout. occurrence 1 lands in bucket b;
    /// occurrence 2 (a near-duplicate, rejected) lands in bucket b+1;
    /// occurrence 3 (a genuine later occurrence) lands in bucket b+2 --
    /// adjacent to occurrence 2's bucket, but two buckets from occurrence
    /// 1's original bucket. Without moving the entry's anchor forward on
    /// each join, occurrence 3's neighbor search (b+1..=b+3) would miss
    /// the entry entirely (still pinned at b) and wrongly start a fresh
    /// count of 1 instead of continuing to 2.
    #[test]
    fn record_follows_a_signal_that_drifts_across_multiple_bucket_boundaries() {
        let mut gate = RepetitionGate::new(FS);

        // Occurrence 1: bucket 140000 (14_000_000 Hz).
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        // Occurrence 2: bucket 140001 (14_000_050 Hz) -- a near-
        // simultaneous duplicate from a different track, rejected, but it
        // joins (and should move the anchor to) bucket 140001.
        // `always_active` is inert: rejected before message-distinctness
        // runs.
        assert_eq!(
            gate.record(2, 14_000_050.0, "K5ARH", 100, 1, always_active),
            1
        );
        // Occurrence 3: bucket 140002 (14_000_200 Hz) -- adjacent to
        // bucket 140001, two away from the original bucket 140000. A
        // genuine later occurrence on a track that reports track 1 (the
        // only track that's actually been accepted so far) as no longer
        // active -- not a duplicate spawn still racing it.
        assert_eq!(
            gate.record(3, 14_000_200.0, "K5ARH", 300_000, 10, |tid| tid != 1),
            2,
            "must follow the anchor across successive adjacent bucket hops, not stay pinned to the original bucket"
        );
    }

    /// Codex review, PR #152, round 9: two entries at b-1 and b+1 each
    /// independently accumulate their OWN single occurrence (two
    /// genuinely distinct real signals, not one drifting one). A
    /// near-simultaneous rejected decode at the empty home bucket b moves
    /// b+1's entry in (a legitimate move, since b was empty). A SECOND
    /// near-simultaneous rejected decode at b-1 must NOT then import b's
    /// entry just because it's fresher -- b-1 already has its own
    /// established identity, and combining the two would silently union
    /// two unrelated signals' histories, manufacturing `reps == 2` when
    /// neither frequency ever produced two distinct occurrences.
    #[test]
    fn record_never_merges_two_independently_established_entries() {
        let mut gate = RepetitionGate::new(FS);

        // b-1 (13_999_900 Hz): a one-off decode, its own entry.
        assert_eq!(
            gate.record(1, 13_999_900.0, "K5ARH", 0, 0, always_active),
            1
        );
        // b+1 (14_000_050 Hz): a separate one-off decode, its own entry.
        assert_eq!(
            gate.record(2, 14_000_050.0, "K5ARH", 0, 0, always_active),
            1
        );
        // A near-simultaneous rejected decode at home bucket b (empty) --
        // legitimately moves the fresher neighbor (b+1) into b.
        // `always_active` is inert: rejected before message-distinctness
        // runs.
        assert_eq!(
            gate.record(3, 14_000_000.0, "K5ARH", 100, 5, always_active),
            1
        );
        // Another near-simultaneous rejected decode, this time at b-1 --
        // which already has its OWN entry. Must use that entry directly,
        // never importing b's (now more recently touched) entry. Rejected
        // (near-duplicate of track 1's own touch), so `always_active` is
        // inert.
        assert_eq!(
            gate.record(4, 13_999_900.0, "K5ARH", 150, 5, always_active),
            1,
            "must never combine two independently-established entries just because one neighbor is fresher"
        );
        assert_eq!(
            gate.len(),
            2,
            "b-1's and b's entries must remain two separate entries, not merged into one"
        );
    }

    /// Codex review, PR #152, round 10: a home entry that's aged past the
    /// trailing window but hasn't been swept yet must be discarded, not
    /// allowed to block a genuinely fresh neighbor -- otherwise round 9's
    /// "always prefer home" rule can pin a decode to dead history and
    /// suppress a valid spot.
    #[test]
    fn an_expired_home_does_not_block_a_fresh_neighbor() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;

        // Home bucket (140000): a stale entry, its only touch at t=0.
        let mut stale = GateEntry::default();
        stale.accepted.push((0, 0, 1, true));
        stale.last_seen_by_track.insert(1, 0);
        gate.seen.insert((140000, "K5ARH".to_string()), stale);

        // Neighbor bucket b+1 (140001): a fresh entry, comfortably within
        // the window as of the decisive call below.
        let fresh_ts = window_samples - 200_000;
        let mut fresh = GateEntry::default();
        fresh.accepted.push((fresh_ts, 0, 2, true));
        fresh.last_seen_by_track.insert(2, fresh_ts);
        gate.seen.insert((140001, "K5ARH".to_string()), fresh);

        // A decode arrives at home (b) just past the window boundary
        // relative to the stale entry (t=0), but still well within the
        // window relative to the fresh neighbor -- on a track that
        // reports track 2 (the fresh neighbor's own toucher) as no longer
        // active, a genuine later occurrence rather than a duplicate spawn.
        let now = window_samples + 1;
        assert_eq!(
            gate.record(3, 14_000_000.0, "K5ARH", now, 10, |tid| tid != 2),
            2,
            "an expired home must be discarded, not block a fresh neighbor's history"
        );
    }

    /// Codex review, PR #152, round 10: with three or more duplicate
    /// tracks reporting one transmission, each staggered just under the
    /// minimum-occurrence gap from the PREVIOUS one but not from the
    /// first accepted occurrence, comparing only against the last
    /// *accepted* timestamp lets the later ones slip through. Track A
    /// accepted at 0s, track B rejected at 0.9s (a near-duplicate of A),
    /// track C at 1.1s -- 1.1s clears the 1.0s gap from A's accepted
    /// timestamp, but C is only 0.2s after B's rejected touch. Must
    /// compare against the entry's most recent activity overall, not just
    /// the last accepted occurrence.
    #[test]
    fn near_duplicate_gap_is_measured_from_the_latest_touch_not_just_the_last_accepted() {
        let mut gate = RepetitionGate::new(FS);
        let one_second = FS as u64;

        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 0, 0, always_active),
            1
        );
        assert_eq!(
            gate.record(
                2,
                14_000_000.0,
                "K5ARH",
                9 * one_second / 10,
                1,
                always_active
            ),
            1,
            "B is a near-duplicate of A, correctly rejected"
        );
        assert_eq!(
            gate.record(
                3,
                14_000_000.0,
                "K5ARH",
                11 * one_second / 10,
                2,
                always_active
            ),
            1,
            "C is only 0.2s after B's rejected touch -- still a likely duplicate of the same occurrence, must not clear the gap just because it's 1.1s past A's accepted timestamp"
        );
    }

    /// Codex review, PR #152, round 13: `Validator::resolve_pending_beacons`
    /// can call `record` with an older, deferred `sample_ts` for a track
    /// well after a newer touch from that same track already landed on
    /// this entry (a long-lived track's Beacon candidate is only judged at
    /// track close). `accepted` is not guaranteed to stay in timestamp
    /// order once that happens -- `most_recent()` must still find the true
    /// latest activity (via `.iter().max()`, not `.last()`), and the
    /// out-of-order replay must never regress the track's own watermark in
    /// `last_seen_by_track`. Without both fixes, a later genuinely-live
    /// touch at the same home bucket can be wrongly judged "expired" and
    /// discarded, destroying real, still-live repetition credit.
    #[test]
    fn an_out_of_order_deferred_replay_does_not_corrupt_the_entrys_recency() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;

        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 8_000_000, 0, always_active),
            1
        );
        assert_eq!(
            gate.record(1, 14_000_000.0, "K5ARH", 8_100_000, 10, always_active),
            2
        );

        // The deferred replay: the SAME track's own much older pending
        // sample_ts, arriving last in call order (simulating
        // resolve_pending_beacons firing at track close). Its own
        // word_seq doesn't matter -- its sample_ts (500) is pruned by the
        // very next call's retain, before message-distinctness is ever
        // computed over it.
        gate.record(1, 14_000_000.0, "K5ARH", 500, 5, always_active);

        // A later, genuinely live touch on a track that reports track 1 as
        // no longer active -- a genuine later signal, not a duplicate
        // spawn. Relative to the old replayed timestamp (500) this looks
        // expired (comfortably past the window), but relative to the
        // entry's TRUE most recent activity (8,100,000) it's still well
        // within the window -- the entry must be recognized as live, not
        // discarded and restarted from 1.
        let now = 8_700_000;
        assert!(
            now - 500 >= window_samples,
            "sanity check: must look expired relative to the stale replayed timestamp"
        );
        assert!(
            now - 8_100_000 < window_samples,
            "sanity check: must still be genuinely live relative to the true latest activity"
        );
        assert_eq!(
            gate.record(4, 14_000_000.0, "K5ARH", now, 20, |tid| tid != 1),
            3,
            "an out-of-order deferred replay must not make a genuinely live entry look expired and discard its real history"
        );
    }
}
