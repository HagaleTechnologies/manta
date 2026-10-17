//! Lock-free per-chunk decode-latency observer (MAN-128). Shared-atomic for
//! the same reason `ListenObservers::active_tracks` is: the decode loop must
//! never block on whatever is consuming this -- `observe` is a bounded
//! linear scan plus three relaxed `fetch_add`s, with no lock.
//!
//! Buckets span the chunk-processing budget at every table rate manta
//! supports: `CHUNK_SAMPLES / fs` ranges from ~10.7 ms at 192 kS/s to
//! ~42.7 ms at 48 kS/s, so the bounds below give resolution on both sides
//! of that range while leaving headroom for a pathological stall.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const DECODE_LATENCY_BUCKETS_SECONDS: [f64; 12] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

/// Snapshot of the observer's state at one instant: non-cumulative bucket
/// counts (length = `DECODE_LATENCY_BUCKETS_SECONDS.len() + 1`, the last
/// being the `+Inf` overflow bucket), the running sum, and the total count.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeLatencySnapshot {
    pub bucket_counts: Vec<u64>,
    pub sum_seconds: f64,
    pub count: u64,
}

pub struct DecodeLatencyObserver {
    buckets: [AtomicU64; DECODE_LATENCY_BUCKETS_SECONDS.len() + 1],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl Default for DecodeLatencyObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodeLatencyObserver {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Records one chunk's processing duration. Linear scan over 12 bounds
    /// (negligible against a 2048-sample chunk's own processing cost),
    /// placing the sample in the first bucket whose bound is >= the value
    /// (Prometheus `le` semantics: the bound is inclusive), or the overflow
    /// bucket if it exceeds every bound.
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        let idx = DECODE_LATENCY_BUCKETS_SECONDS
            .iter()
            .position(|&bound| secs <= bound)
            .unwrap_or(DECODE_LATENCY_BUCKETS_SECONDS.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        // Saturating: a `Duration` that overflows u64 nanoseconds (~584
        // years) can never occur from real chunk timing, but saturating
        // avoids ever panicking in the decode hot loop over it.
        let nanos = d.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DecodeLatencySnapshot {
        DecodeLatencySnapshot {
            bucket_counts: self
                .buckets
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect(),
            sum_seconds: self.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9,
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_places_samples_in_the_first_bucket_whose_bound_is_ge_the_value() {
        let obs = DecodeLatencyObserver::new();
        obs.observe(Duration::from_secs_f64(0.0003));
        obs.observe(Duration::from_secs_f64(0.001)); // exactly a bound: inclusive
        obs.observe(Duration::from_secs_f64(3.0)); // past every bound: overflow

        let snap = obs.snapshot();
        assert_eq!(
            snap.bucket_counts[0], 1,
            "0.0003s belongs in bucket 0 (le 0.0005)"
        );
        assert_eq!(
            snap.bucket_counts[1], 1,
            "exactly 0.001s belongs in bucket 1 (le 0.001)"
        );
        assert_eq!(
            *snap.bucket_counts.last().unwrap(),
            1,
            "3.0s overflows every bound into the last (+Inf) bucket"
        );
        assert_eq!(snap.count, 3);
    }

    #[test]
    fn snapshot_counts_and_sum_match_observations() {
        let obs = DecodeLatencyObserver::new();
        obs.observe(Duration::from_secs_f64(0.001));
        obs.observe(Duration::from_secs_f64(0.002));
        obs.observe(Duration::from_secs_f64(0.003));

        let snap = obs.snapshot();
        assert_eq!(snap.count, 3);
        assert!(
            (snap.sum_seconds - 0.006).abs() < 1e-9,
            "sum_seconds {} should be ~0.006",
            snap.sum_seconds
        );
        let total: u64 = snap.bucket_counts.iter().sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn bucket_bounds_are_strictly_increasing() {
        for i in 1..DECODE_LATENCY_BUCKETS_SECONDS.len() {
            assert!(DECODE_LATENCY_BUCKETS_SECONDS[i] > DECODE_LATENCY_BUCKETS_SECONDS[i - 1]);
        }
    }
}
