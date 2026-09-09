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

/// Minimum gap, in seconds, between two decodes for the later one to
/// count as a genuinely distinct occurrence rather than a near-duplicate
/// (Codex review, PR #152). Neighbor-bucket matching (see `record`'s doc)
/// widens the frequency net two real, simultaneous tracks decoding the
/// SAME over-the-air transmission can fall into -- a known real
/// phenomenon (spectral splatter spawning more than one candidate for one
/// signal) that `merge_converged` doesn't always catch before both reach
/// a full decode. Two such tracks' `WordBoundary`s land within roughly
/// one word's keying duration of each other; a genuine re-transmission
/// (the operator re-keying) is a real pause later. 1.0s sits below any
/// realistic contest CQ repeat cadence while comfortably exceeding a
/// single word's decode duration -- a reasoned choice, not yet measured
/// against real data the way `on_snr_db`/`CHAR_GAP_DITS` were.
const MIN_OCCURRENCE_GAP_SECONDS: f64 = 1.0;

fn bucket(freq_hz: f64) -> i64 {
    (freq_hz / FREQ_BUCKET_HZ).round() as i64
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
    seen: BTreeMap<(i64, String), Vec<u64>>,
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

    /// Records one decode of `callsign` at `freq_hz` at `sample_ts`.
    /// Returns the number of distinct decodes within the trailing window
    /// (including this one).
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
    /// That neighbor search cuts the other way too (same review): it
    /// widens the frequency net enough that two genuinely different,
    /// simultaneous tracks decoding the same real transmission (a known
    /// duplicate-candidate phenomenon) could both land in the same entry
    /// within milliseconds of each other, turning one real transmission
    /// into a false second confirmation. So the minimum-gap check below
    /// applies *only* when the match came from a neighbor bucket, not the
    /// decode's own exact bucket: a legitimate real pattern -- a CQing
    /// station double-calling its own callsign back-to-back within one
    /// transmission ("CQ K5ARH K5ARH K") specifically so the transmission
    /// carries its own two confirmations -- decodes both instances at the
    /// *same* frequency (the exact bucket), and must not be penalized by
    /// a check that exists to catch a *different* signal's track a
    /// neighbor bucket away, not a genuine same-signal repeat. `entry`'s
    /// timestamps are assumed non-decreasing (the pipeline resequences
    /// events by `sample_ts` before they reach here, SPEC §6 rule 6), so
    /// comparing against just the last one is sufficient.
    pub fn record(&mut self, freq_hz: f64, callsign: &str, sample_ts: u64) -> usize {
        self.records_total += 1;
        let b = bucket(freq_hz);
        let home = (b, callsign.to_string());
        let (key, from_neighbor) = if self.seen.contains_key(&home) {
            (home, false)
        } else {
            match [b - 1, b + 1]
                .into_iter()
                .map(|candidate| (candidate, callsign.to_string()))
                .find(|k| self.seen.contains_key(k))
            {
                Some(k) => (k, true),
                None => (home, false),
            }
        };
        let entry = self.seen.entry(key).or_default();
        let is_distinct_occurrence = if from_neighbor {
            match entry.last() {
                Some(&last) => sample_ts.saturating_sub(last) >= self.min_occurrence_gap_samples,
                None => true,
            }
        } else {
            true
        };
        if is_distinct_occurrence {
            entry.push(sample_ts);
        }
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        entry.retain(|&ts| ts >= cutoff);
        entry.len()
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
        self.seen.retain(|_, timestamps| {
            timestamps.retain(|&ts| ts >= cutoff);
            !timestamps.is_empty()
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
        assert_eq!(gate.record(7_080_000.0, "K5ARH", 0), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(7_080_000.0, "K5ARH", 0);
        assert_eq!(gate.record(7_080_000.0, "K5ARH", 300_000), 2);
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(7_080_000.0, "K5ARH", 0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(gate.record(7_080_000.0, "K5ARH", window_samples + 1), 1);
    }

    #[test]
    fn different_frequencies_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(7_080_000.0, "K5ARH", 0);
        // Far enough apart (>1 bucket width) to land outside neighbor matching.
        assert_eq!(gate.record(7_081_000.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(7_080_000.0, "W1AW", 0), 1);
    }

    /// A real signal's track closing and reopening under a new `track_id`
    /// (e.g. `CloseReason::HangExpired`) must not reset its repetition
    /// count -- that's the whole point of keying by frequency bucket
    /// instead of `track_id` (MAN-166). Two `record` calls for the same
    /// bucket+callsign, with a `sweep` between them (as `Validator::ingest`
    /// does on every `TrackClosed`), still accumulate to 2.
    #[test]
    fn sweep_between_two_records_does_not_reset_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(7_080_000.0, "K5ARH", 0);
        gate.sweep(0);
        assert_eq!(gate.record(7_080_000.0, "K5ARH", 300_000), 2);
    }

    /// `sweep` prunes only entries whose timestamps have fully aged out of
    /// the trailing 90s window as of `now_ts` -- a still-live entry must
    /// survive.
    #[test]
    fn sweep_prunes_only_entries_older_than_the_window() {
        let mut gate = RepetitionGate::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;

        gate.record(7_080_000.0, "K5ARH", 0); // will age out, never refreshed
        gate.record(7_090_000.0, "W1AW", 0);
        // A genuine later occurrence (comfortably past both the
        // minimum-occurrence gap and, eventually, `now`'s cutoff) keeps
        // W1AW's entry alive.
        gate.record(7_090_000.0, "W1AW", window_samples + 1);

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
    /// repetition total.
    #[test]
    fn a_decode_just_across_a_bucket_boundary_still_counts_toward_the_same_signal() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(14_000_049.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(14_000_051.0, "K5ARH", 300_000), 2);
    }

    /// Codex review, PR #152: neighbor-bucket matching (added for the
    /// boundary-crossing test above) creates a new risk -- two genuinely
    /// *different*, simultaneous tracks decoding the same real
    /// over-the-air transmission at nearby frequencies (a known real
    /// phenomenon `merge_converged` doesn't always catch in time) would
    /// otherwise both credit the same (bucket, callsign) entry within
    /// milliseconds of each other, inflating one real transmission into a
    /// false `reps == 2` "second confirmation". A decode within
    /// `MIN_OCCURRENCE_GAP_SECONDS` of the entry's most recent timestamp
    /// must not count as a distinct occurrence.
    #[test]
    fn near_simultaneous_decodes_at_nearby_frequencies_do_not_double_count() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(14_000_000.0, "K5ARH", 0), 1); // bucket 140000
                                                              // Same instant, a different (duplicate-spawn) track in the
                                                              // *neighbor* bucket (140001) -- too close in time to be a genuine
                                                              // second occurrence.
        assert_eq!(gate.record(14_000_060.0, "K5ARH", 1_000), 1);
        // A real, later re-transmission (comfortably past the minimum
        // gap) still counts.
        assert_eq!(gate.record(14_000_000.0, "K5ARH", 300_000), 2);
    }

    /// The minimum-gap check above must apply only to a *neighbor*-bucket
    /// match, never to a decode's own exact bucket: a real CQing station
    /// double-calling its own callsign back-to-back within one
    /// transmission ("CQ K5ARH K5ARH K", a deliberate real practice so
    /// the transmission carries its own two confirmations) decodes both
    /// instances at the *same* frequency, often well under a second
    /// apart, and must still count as two.
    #[test]
    fn rapid_same_frequency_repeats_are_never_rejected_as_near_simultaneous() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(14_000_000.0, "K5ARH", 0), 1);
        assert_eq!(gate.record(14_000_000.0, "K5ARH", 500), 2);
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
            gate.record(i as f64 * 1000.0, "K5ARH", 0);
        }
        gate.sweep(window_samples + 1);
        assert_eq!(
            gate.seen.len(),
            0,
            "seen must not accumulate one entry per historical bucket once swept past the window"
        );
    }
}
