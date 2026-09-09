//! Repetition gate: a callsign must decode >= 2 distinct times within a
//! 90 s window on its track before first spot. SPEC §4.6 / ARCHITECTURE
//! §6.4. `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule
//! 2). `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly
//! into whether/when a `Spot` is emitted.

use std::collections::BTreeMap;

const WINDOW_SECONDS: f64 = 90.0;

pub struct RepetitionGate {
    window_samples: u64,
    /// Keyed by a frequency bucket (not `track_id`, MAN-166): a real
    /// signal's `track_id` changes every time its track closes and
    /// reopens (e.g. `CloseReason::HangExpired`'s 5s silence timer), so
    /// keying repetition memory by `track_id` defeated this gate's own
    /// 90s window the instant a track churned, regardless of whether the
    /// same callsign was still genuinely repeating. A frequency bucket
    /// survives that churn; see `Validator`'s `freq_bucket` for how the
    /// bucket is derived.
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
            seen: BTreeMap::new(),
            records_total: 0,
        }
    }

    /// Records one decode of `callsign` at `freq_bucket` at `sample_ts`.
    /// Returns the number of distinct decodes within the trailing window
    /// (including this one).
    pub fn record(&mut self, freq_bucket: i64, callsign: &str, sample_ts: u64) -> usize {
        self.records_total += 1;
        let entry = self
            .seen
            .entry((freq_bucket, callsign.to_string()))
            .or_default();
        entry.push(sample_ts);
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        entry.retain(|&ts| ts >= cutoff);
        entry.len()
    }

    /// See `records_total`'s doc.
    pub fn records_total(&self) -> u64 {
        self.records_total
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
        assert_eq!(gate.record(70_800, "K5ARH", 0), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(70_800, "K5ARH", 0);
        assert_eq!(gate.record(70_800, "K5ARH", 100_000), 2);
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(70_800, "K5ARH", 0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(gate.record(70_800, "K5ARH", window_samples + 1), 1);
    }

    #[test]
    fn different_frequency_buckets_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(70_800, "K5ARH", 0);
        assert_eq!(gate.record(70_801, "K5ARH", 0), 1);
        assert_eq!(gate.record(70_800, "W1AW", 0), 1);
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
        gate.record(70_800, "K5ARH", 0);
        gate.sweep(0);
        assert_eq!(gate.record(70_800, "K5ARH", 100_000), 2);
    }

    /// `sweep` prunes only entries whose timestamps have fully aged out of
    /// the trailing 90s window as of `now_ts` -- a still-live entry must
    /// survive.
    #[test]
    fn sweep_prunes_only_entries_older_than_the_window() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(70_800, "K5ARH", 0); // will age out
        gate.record(70_900, "W1AW", 0); // will still be live

        let window_samples = (WINDOW_SECONDS * FS) as u64;
        let now = window_samples + 1;
        gate.record(70_900, "W1AW", now); // refreshes W1AW's timestamp
        gate.sweep(now);

        assert!(
            !gate.seen.contains_key(&(70_800, "K5ARH".to_string())),
            "K5ARH's only decode is older than the window and must be pruned"
        );
        assert!(
            gate.seen.contains_key(&(70_900, "W1AW".to_string())),
            "W1AW was just recorded at `now` and must survive"
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
        for bucket in 0..10_000i64 {
            gate.record(bucket, "K5ARH", 0);
        }
        gate.sweep(window_samples + 1);
        assert_eq!(
            gate.seen.len(),
            0,
            "seen must not accumulate one entry per historical bucket once swept past the window"
        );
    }
}
