//! Repetition gate: a callsign must decode >= 2 distinct times within a
//! 90 s window on its track before first spot. SPEC §4.6 / ARCHITECTURE
//! §6.4. `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule
//! 2). `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly
//! into whether/when a `Spot` is emitted.

use std::collections::BTreeMap;

const WINDOW_SECONDS: f64 = 90.0;

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
const MIN_OCCURRENCE_GAP_SECONDS: f64 = 1.0;

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
    /// Timestamps of *accepted* (distinct) occurrences -- what actually
    /// drives the returned repetition count.
    accepted: Vec<u64>,
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
    fn most_recent(&self) -> Option<u64> {
        self.accepted
            .last()
            .copied()
            .into_iter()
            .chain(self.last_seen_by_track.values().copied())
            .max()
    }
}

pub struct RepetitionGate {
    window_samples: u64,
    min_occurrence_gap_samples: u64,
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
            seen: BTreeMap::new(),
            records_total: 0,
        }
    }

    /// Records one decode of `callsign` by `track_id` at `freq_hz` at
    /// `sample_ts`. Returns the number of distinct decodes within the
    /// trailing window (including this one).
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
    pub fn record(&mut self, track_id: u32, freq_hz: f64, callsign: &str, sample_ts: u64) -> usize {
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
        entry.last_seen_by_track.insert(track_id, sample_ts);
        if is_distinct_occurrence {
            entry.accepted.push(sample_ts);
        }
        entry.accepted.retain(|&ts| ts >= cutoff);
        entry.last_seen_by_track.retain(|_, ts| *ts >= cutoff);
        entry.accepted.len()
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
            entry.accepted.retain(|&ts| ts >= cutoff);
            entry.last_seen_by_track.retain(|_, ts| *ts >= cutoff);
            !entry.accepted.is_empty() || !entry.last_seen_by_track.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_decode_counts_as_one() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 0), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0);
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", 300_000), 2);
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(gate.record(1, 7_080_000.0, "K5ARH", window_samples + 1), 1);
    }

    #[test]
    fn different_frequencies_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0);
        // Far enough apart (>1 bucket width) to land outside neighbor matching.
        assert_eq!(gate.record(1, 7_081_000.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(1, 7_080_000.0, "W1AW", 0), 1);
    }

    /// A real signal's track closing and reopening under a new `track_id`
    /// (e.g. `CloseReason::HangExpired`) must not reset its repetition
    /// count -- that's the whole point of keying by frequency bucket
    /// instead of `track_id` (MAN-166). Two `record` calls for the same
    /// bucket+callsign under *different* track_ids (simulating the close
    /// + reopen), with a `sweep` between them (as `Validator::ingest`
    /// does on every `TrackClosed`), still accumulate to 2.
    #[test]
    fn sweep_between_two_records_does_not_reset_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, 7_080_000.0, "K5ARH", 0);
        gate.sweep(0);
        assert_eq!(gate.record(2, 7_080_000.0, "K5ARH", 300_000), 2);
    }

    /// `sweep` prunes only entries whose timestamps have fully aged out of
    /// the trailing 90s window as of `now_ts` -- a still-live entry must
    /// survive.
    #[test]
    fn sweep_prunes_only_entries_older_than_the_window() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;

        gate.record(1, 7_080_000.0, "K5ARH", 0); // will age out, never refreshed
        gate.record(2, 7_090_000.0, "W1AW", 0);
        // A genuine later occurrence (comfortably past both the
        // minimum-occurrence gap and, eventually, `now`'s cutoff) keeps
        // W1AW's entry alive.
        gate.record(2, 7_090_000.0, "W1AW", window_samples + 1);

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
    /// drifted closing and reopening).
    #[test]
    fn a_decode_just_across_a_bucket_boundary_still_counts_toward_the_same_signal() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 14_000_049.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(2, 14_000_051.0, "K5ARH", 300_000), 2);
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
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(2, 14_000_000.0, "K5ARH", 1_000), 1);
        // Neighbor bucket, yet another track, still the same instant.
        assert_eq!(gate.record(3, 14_000_060.0, "K5ARH", 1_500), 1);
        // A real, later re-transmission (comfortably past the minimum
        // gap; track_id doesn't matter here) still counts.
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 300_000), 2);
    }

    /// The minimum-gap check above must never apply *within* a single
    /// track: a real CQing station double-calling its own callsign
    /// back-to-back within one transmission ("CQ K5ARH K5ARH K", a
    /// deliberate real practice so the transmission carries its own two
    /// confirmations) decodes both instances on the same track, often
    /// well under a second apart, and must still count as two -- one
    /// continuous decode stream can't decode the same instant twice.
    #[test]
    fn rapid_same_track_repeats_are_never_rejected_as_near_simultaneous() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 500), 2);
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
            assert_eq!(gate.record(1, freq, "K5ARH", 0), 0);
        }
        assert!(
            gate.is_empty(),
            "non-finite frequencies must never create an entry"
        );
        for freq in [f64::MAX, f64::MIN] {
            gate.record(1, freq, "K5ARH", 0);
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
        gate.record(1, 14_999_900.0, "K5ARH", 0);
        // Bucket b+1 (15_000_100 Hz): a more recent decode, different track.
        gate.record(2, 15_000_100.0, "K5ARH", 500_000);

        // A new decode at the home bucket (15_000_000 Hz), comfortably past
        // the minimum-occurrence gap from bucket b+1's timestamp, must join
        // the freshest neighbor (b+1) -- becoming its second occurrence --
        // not the older, lowest-numbered one (b-1).
        assert_eq!(
            gate.record(3, 15_000_000.0, "K5ARH", 700_000),
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
    #[test]
    fn a_tracks_own_repeat_counts_even_after_its_first_attempt_was_rejected() {
        let mut gate = RepetitionGate::new(FS);
        // Track 1 establishes the entry.
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        // Track 2's near-simultaneous decode is correctly rejected as a
        // likely duplicate of track 1's.
        assert_eq!(gate.record(2, 14_000_000.0, "K5ARH", 500), 1);
        // Track 2 decodes AGAIN, shortly after its own (rejected) first
        // attempt -- this is track 2's own second word, not a duplicate
        // of anyone else, and must count as a second distinct occurrence
        // even though it's still well under the minimum gap from track
        // 1's original timestamp.
        assert_eq!(gate.record(2, 14_000_000.0, "K5ARH", 600), 2);
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
            gate.record(i as u32, i as f64 * 1000.0, "K5ARH", 0);
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

        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        // Track B's near-simultaneous duplicate of A's first decode is
        // correctly rejected -- but B's identity is now on record.
        assert_eq!(gate.record(2, 14_000_000.0, "K5ARH", one_second / 2), 1);

        // Much later, but still inside the trailing 90s window: track A's
        // genuine second occurrence.
        let later = 89 * one_second;
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", later), 2);

        // Immediately after: track B decodes again. This is near-
        // simultaneous with A's fresh occurrence above, not with B's own
        // ancient first touch (88+ seconds earlier) -- must be rejected
        // as a likely duplicate of A's just-accepted occurrence, not
        // waved through as "B's own repeat."
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", later + one_second / 20),
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
        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        // Occurrence 2: bucket 140001 (14_000_050 Hz) -- a near-
        // simultaneous duplicate from a different track, rejected, but it
        // joins (and should move the anchor to) bucket 140001.
        assert_eq!(gate.record(2, 14_000_050.0, "K5ARH", 100), 1);
        // Occurrence 3: bucket 140002 (14_000_200 Hz) -- adjacent to
        // bucket 140001, two away from the original bucket 140000. A
        // genuine later occurrence, comfortably past the
        // minimum-occurrence gap.
        assert_eq!(
            gate.record(3, 14_000_200.0, "K5ARH", 300_000),
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
        assert_eq!(gate.record(1, 13_999_900.0, "K5ARH", 0), 1);
        // b+1 (14_000_050 Hz): a separate one-off decode, its own entry.
        assert_eq!(gate.record(2, 14_000_050.0, "K5ARH", 0), 1);
        // A near-simultaneous rejected decode at home bucket b (empty) --
        // legitimately moves the fresher neighbor (b+1) into b.
        assert_eq!(gate.record(3, 14_000_000.0, "K5ARH", 100), 1);
        // Another near-simultaneous rejected decode, this time at b-1 --
        // which already has its OWN entry. Must use that entry directly,
        // never importing b's (now more recently touched) entry.
        assert_eq!(
            gate.record(4, 13_999_900.0, "K5ARH", 150),
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
        stale.accepted.push(0);
        stale.last_seen_by_track.insert(1, 0);
        gate.seen.insert((140000, "K5ARH".to_string()), stale);

        // Neighbor bucket b+1 (140001): a fresh entry, comfortably within
        // the window as of the decisive call below.
        let fresh_ts = window_samples - 200_000;
        let mut fresh = GateEntry::default();
        fresh.accepted.push(fresh_ts);
        fresh.last_seen_by_track.insert(2, fresh_ts);
        gate.seen.insert((140001, "K5ARH".to_string()), fresh);

        // A decode arrives at home (b) just past the window boundary
        // relative to the stale entry (t=0), but still well within the
        // window relative to the fresh neighbor.
        let now = window_samples + 1;
        assert_eq!(
            gate.record(3, 14_000_000.0, "K5ARH", now),
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

        assert_eq!(gate.record(1, 14_000_000.0, "K5ARH", 0), 1);
        assert_eq!(
            gate.record(2, 14_000_000.0, "K5ARH", 9 * one_second / 10),
            1,
            "B is a near-duplicate of A, correctly rejected"
        );
        assert_eq!(
            gate.record(3, 14_000_000.0, "K5ARH", 11 * one_second / 10),
            1,
            "C is only 0.2s after B's rejected touch -- still a likely duplicate of the same occurrence, must not clear the gap just because it's 1.1s past A's accepted timestamp"
        );
    }
}
