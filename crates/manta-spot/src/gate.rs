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
    /// apart). So a new decode first checks its own bucket *and* both
    /// neighbors for an existing entry under the same callsign, and joins
    /// that one if found, rather than always keying strictly by its own
    /// freshly-computed bucket.
    ///
    /// That widened net cuts the other way too (same review): two
    /// genuinely different, simultaneous tracks decoding the same real
    /// transmission (a known duplicate-candidate phenomenon
    /// `merge_converged` doesn't always catch in time) could land in the
    /// same bucket -- the exact one, not just a neighbor -- and both
    /// credit the same entry within milliseconds of each other. `track_id`
    /// is what actually distinguishes that from a real pattern (a CQing
    /// station double-calling its own callsign back-to-back within one
    /// transmission): see `MIN_OCCURRENCE_GAP_SECONDS`'s doc. Whether
    /// *this* track has touched the entry before -- not just whether the
    /// single most-recent accepted occurrence happened to be from it --
    /// is what "same track" means here (`GateEntry::last_seen_by_track`'s
    /// doc): a track's own first decode, even if rejected as a
    /// near-duplicate of a different track's, must not block that same
    /// track's own later, genuinely distinct repeat.
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
        // Among the home bucket and both neighbors, join whichever
        // existing entry is *freshest* (see `GateEntry::most_recent`),
        // not just the lowest-numbered one that happens to still exist --
        // sweeps are throttled, so a stale-but-not-yet-swept neighbor
        // must not be preferred over a genuinely fresher one.
        let key = (b - 1..=b + 1)
            .map(|candidate| (candidate, callsign.to_string()))
            .filter_map(|k| {
                let ts = self.seen.get(&k)?.most_recent()?;
                Some((k, ts))
            })
            .max_by_key(|(_, ts)| *ts)
            .map(|(k, _)| k)
            .unwrap_or((b, callsign.to_string()));
        let entry = self.seen.entry(key).or_default();
        let is_own_track_before = entry.last_seen_by_track.contains_key(&track_id);
        let is_distinct_occurrence = if is_own_track_before {
            true
        } else {
            match entry.accepted.last() {
                Some(&last) => sample_ts.saturating_sub(last) >= self.min_occurrence_gap_samples,
                None => true,
            }
        };
        entry.last_seen_by_track.insert(track_id, sample_ts);
        if is_distinct_occurrence {
            entry.accepted.push(sample_ts);
        }
        let cutoff = sample_ts.saturating_sub(self.window_samples);
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
}
