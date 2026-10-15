//! Re-spot suppression. ARCHITECTURE §6.5. `sample_ts`-based (SPEC
//! -decode-core.md §6 rule 2). `BTreeMap`, never `HashMap` (rule 3).
//!
//! MAN-13 (D6): with several inputs feeding one validator, a report from a
//! **different input** than the stored spot's is a duplicate for the whole
//! suppression window when it falls in the same 300 Hz bucket, or within
//! one channel ([`CROSS_INPUT_MAX_HZ`]) of the stored spot across a bucket
//! edge. No SNR or spot-type override applies across inputs: two receivers
//! hearing one transmission must not spot it twice just because one antenna
//! hears it 6 dB louder. A report from the stored spot's own input keeps
//! the single-input rule unchanged. The stored timestamp never moves
//! backwards: an override emitted with an older `sample_ts` (another
//! input's batch merged late) keeps the newer one.

use crate::context::SpotType;
use std::collections::BTreeMap;

/// MAN-13 (D6): furthest, in Hz, a different input's report may sit from a
/// stored spot in a neighbouring bucket and still be the same signal: one
/// channel, the gate's own same-signal rule.
pub const CROSS_INPUT_MAX_HZ: f64 = crate::gate::SAME_SIGNAL_MAX_HZ;

const FREQ_BUCKET_HZ: f64 = 300.0;
const SUPPRESSION_SECONDS: f64 = 600.0;
const SNR_IMPROVEMENT_DB: f32 = 6.0;

struct LastSpot {
    sample_ts: u64,
    snr_db: f32,
    spot_type: SpotType,
    /// MAN-13: the input that reported the stored spot.
    input: u32,
    /// MAN-13: the stored spot's frequency, for the cross-bucket match.
    freq_hz: f64,
}

pub struct Dedupe {
    suppression_window_samples: u64,
    last: BTreeMap<(String, i64), LastSpot>,
}

impl Dedupe {
    pub fn new(fs: f64) -> Self {
        Self {
            suppression_window_samples: (SUPPRESSION_SECONDS * fs) as u64,
            last: BTreeMap::new(),
        }
    }

    fn bucket(freq_hz: f64) -> i64 {
        (freq_hz / FREQ_BUCKET_HZ).round() as i64
    }

    /// Read-only peek at the currently-recorded last-spot timestamp for
    /// this `(callsign, freq_hz)` identity, if any -- lets a caller with
    /// its own delayed/deferred observation detect when that observation
    /// is already stale relative to newer activity, without itself
    /// mutating dedupe state. Codex review on PR #154, round 8:
    /// `manta-spot::Validator`'s deferred Beacon candidates (resolved at
    /// `TrackClosed`, possibly well after capture) must never call
    /// `should_emit` with a `sample_ts` older than what's already
    /// recorded here -- that would silently roll the watermark backward
    /// and let a later real spot escape the suppression window early.
    pub(crate) fn last_sample_ts(&self, callsign: &str, freq_hz: f64) -> Option<u64> {
        let key = (callsign.to_string(), Self::bucket(freq_hz));
        self.last.get(&key).map(|s| s.sample_ts)
    }

    /// True if a spot for this `(callsign, freq_hz)` should be emitted now
    /// -- no prior spot, the suppression window has elapsed, SNR improved
    /// by at least `SNR_IMPROVEMENT_DB`, or the spot type changed. Records
    /// the new spot as the latest one when it returns true.
    ///
    /// `input` is the reporting input's index (always 0 with one input).
    /// A report from a different input than the stored spot's is a
    /// duplicate for the whole window, with no SNR or type override, in the
    /// same bucket or within `CROSS_INPUT_MAX_HZ` across a bucket edge
    /// (MAN-13, D6; see the module doc).
    pub fn should_emit(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        snr_db: f32,
        spot_type: SpotType,
        sample_ts: u64,
        input: u32,
    ) -> bool {
        let bucket = Self::bucket(freq_hz);
        let key = (callsign.to_string(), bucket);
        let window = self.suppression_window_samples;
        let in_window = |prev: &LastSpot| sample_ts.saturating_sub(prev.sample_ts) < window;
        let emit = match self.last.get(&key) {
            None => {
                // MAN-13: a different input's spot one bucket over, within
                // one channel, is the same signal straddling the edge.
                let neighbour = if freq_hz / FREQ_BUCKET_HZ >= bucket as f64 {
                    bucket + 1
                } else {
                    bucket - 1
                };
                !self
                    .last
                    .get(&(callsign.to_string(), neighbour))
                    .is_some_and(|prev| {
                        prev.input != input
                            && (freq_hz - prev.freq_hz).abs() < CROSS_INPUT_MAX_HZ
                            && in_window(prev)
                    })
            }
            Some(prev) if prev.input != input => !in_window(prev),
            Some(prev) => {
                !in_window(prev)
                    || snr_db - prev.snr_db >= SNR_IMPROVEMENT_DB
                    || spot_type != prev.spot_type
            }
        };
        if emit {
            let stored_ts = self
                .last
                .get(&key)
                .map_or(sample_ts, |prev| prev.sample_ts.max(sample_ts));
            self.last.insert(
                key,
                LastSpot {
                    sample_ts: stored_ts,
                    snr_db,
                    spot_type,
                    input,
                    freq_hz,
                },
            );
        }
        emit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_spot_always_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0));
    }

    #[test]
    fn immediate_repeat_same_snr_and_type_is_suppressed() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0);
        assert!(!d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 1000, 0));
    }

    #[test]
    fn snr_jump_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 26.0, SpotType::Cq, 1000, 0));
    }

    #[test]
    fn type_change_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::De, 1000, 0));
    }

    #[test]
    fn window_elapsing_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0);
        let window_samples = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit(
            "K5ARH",
            14_027_000.0,
            20.0,
            SpotType::Cq,
            window_samples + 1,
            0
        ));
    }

    #[test]
    fn different_freq_bucket_is_independent() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0, 0);
        assert!(d.should_emit("K5ARH", 14_030_000.0, 20.0, SpotType::Cq, 1000, 0));
    }

    // MAN-13 (D6): cross-input overlap dedupe.

    #[test]
    fn another_inputs_report_in_the_same_bucket_is_a_duplicate_even_6db_stronger() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 0, 0));
        assert!(!d.should_emit("W1AW", 14_012_345.0, 30.0, SpotType::Cq, 1000, 1));
    }

    #[test]
    fn another_inputs_report_with_a_new_type_is_a_duplicate_within_the_window() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 0, 0));
        assert!(!d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::De, 1000, 1));
    }

    #[test]
    fn another_inputs_report_across_a_bucket_edge_within_one_channel_is_a_duplicate() {
        // The measured 3 Hz receiver disagreement (14_012_349.9 vs
        // 14_012_352.7 Hz), shifted so the two straddle the 300 Hz edge at
        // 14_012_550 Hz (bucket 46_708.5).
        let mut d = Dedupe::new(FS);
        let a = 14_012_549.9 - 1.5;
        let b = 14_012_552.7 - 1.5;
        assert_ne!(Dedupe::bucket(a), Dedupe::bucket(b));
        assert!(d.should_emit("W1AW", a, 20.0, SpotType::Cq, 0, 0));
        assert!(!d.should_emit("W1AW", b, 20.0, SpotType::Cq, 1000, 1));
        // And the other way round.
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", b, 20.0, SpotType::Cq, 0, 1));
        assert!(!d.should_emit("W1AW", a, 20.0, SpotType::Cq, 1000, 0));
    }

    #[test]
    fn another_inputs_report_across_a_bucket_edge_more_than_one_channel_away_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_500.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_600.0, 20.0, SpotType::Cq, 1000, 1));
    }

    #[test]
    fn another_inputs_report_after_the_window_emits() {
        let mut d = Dedupe::new(FS);
        let window_samples = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, window_samples, 1));
    }

    #[test]
    fn same_input_snr_and_type_overrides_are_unchanged() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 0, 2));
        assert!(d.should_emit("W1AW", 14_012_340.0, 26.0, SpotType::Cq, 1000, 2));
        assert!(d.should_emit("W1AW", 14_012_340.0, 26.0, SpotType::De, 2000, 2));
        assert!(!d.should_emit("W1AW", 14_012_340.0, 26.0, SpotType::De, 3000, 2));
    }

    #[test]
    fn an_override_with_an_older_sample_ts_never_rewinds_the_stored_time() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 50_000, 0));
        // A late-merged, older report that still emits through an SNR jump.
        assert!(d.should_emit("W1AW", 14_012_340.0, 30.0, SpotType::Cq, 10_000, 0));
        assert_eq!(d.last_sample_ts("W1AW", 14_012_340.0), Some(50_000));
    }
}
