//! Lock-free decode-quality observer (MAN-118): characters decoded,
//! garbles, each decoding track's keying-rail ratio once per second, and
//! the repetition gate's result distribution, and the validator's
//! decisions by reason (MAN-207). Shared-atomic for the same
//! reason `latency.rs` is: the decode loop must never block on whatever is
//! consuming this. `observe_rail_ratio` runs on `drain_pool`'s rayon
//! workers, so every method takes `&self` and uses relaxed atomics only.
//!
//! Write-only from the decode path: nothing here is read back into
//! decoding, so file replay stays byte-identical with or without it.
use manta_spot::decision::{Reason, Verdict};
use manta_spot::observation::ObservationCounts;
use manta_spot::{RepetitionCounts, REPETITION_BUCKETS};

/// Spotted, plus one slot per `Reason` (MAN-207).
const DECISION_SLOTS: usize = 1 + Reason::ALL.len();
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds of the `manta_track_rail_ratio` histogram's buckets. The
/// rails never fall below a ratio of 2 (`Demod::rail_ratio`), so `le="2"`
/// counts samples at that collapse floor; the rest span the measured range
/// from deep fading (V5's median 3.3) to a clean strong signal (VR3's
/// median 30) with headroom.
pub const RAIL_RATIO_BUCKETS: [f64; 10] = [2.0, 2.5, 3.0, 4.0, 6.0, 10.0, 16.0, 25.0, 40.0, 63.0];

/// Snapshot of the observer at one instant. Histogram bucket counts are
/// non-cumulative, with a final `+Inf` overflow slot.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeQualitySnapshot {
    pub chars_total: u64,
    pub garbles_total: u64,
    /// Length `RAIL_RATIO_BUCKETS.len() + 1`.
    pub rail_ratio_bucket_counts: Vec<u64>,
    pub rail_ratio_sum: f64,
    pub rail_ratio_count: u64,
    /// Length `REPETITION_BUCKETS.len() + 1`.
    pub repetition_bucket_counts: Vec<u64>,
    pub repetition_sum: u64,
    /// MAN-207: every valid (verdict, reason) pair with its count, in
    /// `ObservationCounts::iter` order (`None` is a spot).
    pub decisions: Vec<(Verdict, Option<Reason>, u64)>,
}

pub struct DecodeQualityObserver {
    chars: AtomicU64,
    garbles: AtomicU64,
    rail_buckets: [AtomicU64; RAIL_RATIO_BUCKETS.len() + 1],
    /// `f64` bits; added to with a compare-and-swap loop.
    rail_sum_bits: AtomicU64,
    rail_count: AtomicU64,
    repetition_buckets: [AtomicU64; REPETITION_BUCKETS.len() + 1],
    repetition_sum: AtomicU64,
    /// MAN-207: in `ObservationCounts::iter` order.
    decisions: [AtomicU64; DECISION_SLOTS],
}

impl Default for DecodeQualityObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodeQualityObserver {
    pub fn new() -> Self {
        Self {
            chars: AtomicU64::new(0),
            garbles: AtomicU64::new(0),
            rail_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            rail_sum_bits: AtomicU64::new(0.0f64.to_bits()),
            rail_count: AtomicU64::new(0),
            repetition_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            repetition_sum: AtomicU64::new(0),
            decisions: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Adds `n` `CharDecoded` events.
    pub fn add_chars(&self, n: u64) {
        self.chars.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n` garbles (`TrackDecoder::garble_count` increments).
    pub fn add_garbles(&self, n: u64) {
        self.garbles.fetch_add(n, Ordering::Relaxed);
    }

    /// Records one track's rail ratio in the first bucket whose bound is at
    /// least the value (Prometheus `le` semantics), or the overflow bucket.
    /// A non-finite value is dropped rather than poisoning the sum.
    pub fn observe_rail_ratio(&self, ratio: f32) {
        let r = f64::from(ratio);
        if !r.is_finite() {
            return;
        }
        let idx = RAIL_RATIO_BUCKETS
            .iter()
            .position(|&bound| r <= bound)
            .unwrap_or(RAIL_RATIO_BUCKETS.len());
        self.rail_buckets[idx].fetch_add(1, Ordering::Relaxed);
        // The closure always returns `Some`, so this never fails.
        let _ = self
            .rail_sum_bits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + r).to_bits())
            });
        self.rail_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Overwrites the repetition distribution with the validator's own,
    /// which is already cumulative for the life of the `listen()` call.
    pub fn store_repetitions(&self, counts: RepetitionCounts) {
        for (slot, &n) in self.repetition_buckets.iter().zip(&counts.bucket_counts) {
            slot.store(n, Ordering::Relaxed);
        }
        self.repetition_sum.store(counts.sum, Ordering::Relaxed);
    }

    /// MAN-207: overwrites the decision counts with the validator's own,
    /// which are already cumulative for the life of the `listen()` call.
    pub fn store_decisions(&self, counts: &ObservationCounts) {
        for (slot, (_, _, n)) in self.decisions.iter().zip(counts.iter()) {
            slot.store(n, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> DecodeQualitySnapshot {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        DecodeQualitySnapshot {
            chars_total: load(&self.chars),
            garbles_total: load(&self.garbles),
            rail_ratio_bucket_counts: self.rail_buckets.iter().map(load).collect(),
            rail_ratio_sum: f64::from_bits(load(&self.rail_sum_bits)),
            rail_ratio_count: load(&self.rail_count),
            repetition_bucket_counts: self.repetition_buckets.iter().map(load).collect(),
            repetition_sum: load(&self.repetition_sum),
            decisions: ObservationCounts::default()
                .iter()
                .zip(&self.decisions)
                .map(|((verdict, reason, _), n)| (verdict, reason, load(n)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_ratio_buckets_are_le_inclusive_with_an_overflow_slot() {
        let obs = DecodeQualityObserver::new();
        obs.observe_rail_ratio(2.0); // exactly the floor bound: bucket 0
        obs.observe_rail_ratio(2.2); // le 2.5
        obs.observe_rail_ratio(70.0); // past 63: overflow
        let snap = obs.snapshot();
        assert_eq!(
            snap.rail_ratio_bucket_counts.len(),
            RAIL_RATIO_BUCKETS.len() + 1
        );
        assert_eq!(snap.rail_ratio_bucket_counts[0], 1);
        assert_eq!(snap.rail_ratio_bucket_counts[1], 1);
        assert_eq!(*snap.rail_ratio_bucket_counts.last().unwrap(), 1);
        assert_eq!(snap.rail_ratio_count, 3);
        assert!(
            (snap.rail_ratio_sum - (2.0 + f64::from(2.2f32) + 70.0)).abs() < 1e-9,
            "rail sum {}",
            snap.rail_ratio_sum
        );
    }

    #[test]
    fn a_non_finite_rail_ratio_is_dropped() {
        let obs = DecodeQualityObserver::new();
        obs.observe_rail_ratio(f32::NAN);
        obs.observe_rail_ratio(f32::INFINITY);
        let snap = obs.snapshot();
        assert_eq!(snap.rail_ratio_count, 0);
        assert_eq!(snap.rail_ratio_sum, 0.0);
    }

    #[test]
    fn counters_accumulate() {
        let obs = DecodeQualityObserver::new();
        obs.add_chars(5);
        obs.add_chars(2);
        obs.add_garbles(1);
        let snap = obs.snapshot();
        assert_eq!(snap.chars_total, 7);
        assert_eq!(snap.garbles_total, 1);
    }

    #[test]
    fn store_decisions_overwrites_rather_than_adds() {
        let obs = DecodeQualityObserver::new();
        let fresh = obs.snapshot().decisions;
        assert_eq!(fresh.len(), 1 + Reason::ALL.len());
        assert!(fresh.iter().all(|&(_, _, n)| n == 0));
        assert_eq!(fresh[0], (Verdict::Spotted, None, 0));

        let mut log =
            manta_spot::ObservationLog::new(1000.0, manta_spot::ObservationPolicy::summary());
        log.count_decision(Some(Reason::Repetition));
        log.count_decision(Some(Reason::Repetition));
        obs.store_decisions(log.counts());
        log.count_decision(None);
        obs.store_decisions(log.counts());
        let snap = obs.snapshot();
        let count = |reason| {
            snap.decisions
                .iter()
                .find(|&&(_, r, _)| r == reason)
                .map(|&(_, _, n)| n)
        };
        assert_eq!(count(Some(Reason::Repetition)), Some(2));
        assert_eq!(count(None), Some(1));
        assert_eq!(snap.decisions.iter().map(|&(_, _, n)| n).sum::<u64>(), 3);
    }

    #[test]
    fn store_repetitions_overwrites_rather_than_adds() {
        let obs = DecodeQualityObserver::new();
        let mut first = RepetitionCounts::default();
        first.bucket_counts[0] = 4;
        first.sum = 4;
        obs.store_repetitions(first);
        let mut second = RepetitionCounts::default();
        second.bucket_counts[0] = 5;
        second.bucket_counts[1] = 1;
        second.sum = 7;
        obs.store_repetitions(second);
        let snap = obs.snapshot();
        assert_eq!(snap.repetition_bucket_counts, second.bucket_counts.to_vec());
        assert_eq!(snap.repetition_sum, 7);
    }

    #[test]
    fn concurrent_rail_observations_lose_nothing() {
        let obs = std::sync::Arc::new(DecodeQualityObserver::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let obs = obs.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        obs.observe_rail_ratio(3.0);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let snap = obs.snapshot();
        assert_eq!(snap.rail_ratio_count, 4000);
        assert_eq!(snap.rail_ratio_sum, 12_000.0);
    }

    #[test]
    fn bucket_bounds_are_strictly_increasing() {
        for i in 1..RAIL_RATIO_BUCKETS.len() {
            assert!(RAIL_RATIO_BUCKETS[i] > RAIL_RATIO_BUCKETS[i - 1]);
        }
    }
}
