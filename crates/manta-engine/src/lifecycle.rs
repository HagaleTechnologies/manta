//! Track and gate counters from the streaming loop (MAN-236).
//!
//! Includes every track-map entry, even unconfirmed candidates. Relaxed
//! atomics keep the decode loop independent of readers; nothing here feeds
//! back into decoding. Fields are individually current, with no cross-field
//! snapshot guarantee for readers on another thread. Use one fresh observer
//! per run, with the streaming loop as its only writer.

use crate::track::CloseCounts;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counters published by one streaming run, including all outage segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackLifecycleSnapshot {
    /// Steady-state input chunks processed after the startup calibration fill.
    pub chunks: u64,
    /// Every open track after the latest steady-state chunk; zero at EOF
    /// or a source read error. Includes unconfirmed candidates.
    pub active_tracks: u64,
    /// Largest all-entry count after a steady-state chunk or just before an
    /// EOF/discontinuity flush. Excludes padding and calibration batches.
    pub peak_active_tracks: u64,
    /// Per-reason closes, summed across discontinuity segments.
    pub close_counts: CloseCounts,
    /// Cumulative `Validator::gate_records_total` for the run.
    pub gate_records_total: u64,
}

/// Atomics-only observer for the counters needed by `soak_with_metrics`.
#[derive(Debug, Default)]
pub struct TrackLifecycleObserver {
    chunks: AtomicU64,
    active_tracks: AtomicU64,
    peak_active_tracks: AtomicU64,
    unconfirmed: AtomicU64,
    hang_expired: AtomicU64,
    silent: AtomicU64,
    merged: AtomicU64,
    evicted: AtomicU64,
    rtty: AtomicU64,
    not_cw: AtomicU64,
    unkeyed: AtomicU64,
    gate_records_total: AtomicU64,
}

impl TrackLifecycleObserver {
    /// Reads each field independently with relaxed loads.
    pub fn snapshot(&self) -> TrackLifecycleSnapshot {
        TrackLifecycleSnapshot {
            chunks: self.chunks.load(Ordering::Relaxed),
            active_tracks: self.active_tracks.load(Ordering::Relaxed),
            peak_active_tracks: self.peak_active_tracks.load(Ordering::Relaxed),
            close_counts: CloseCounts {
                unconfirmed: self.unconfirmed.load(Ordering::Relaxed),
                hang_expired: self.hang_expired.load(Ordering::Relaxed),
                silent: self.silent.load(Ordering::Relaxed),
                merged: self.merged.load(Ordering::Relaxed),
                evicted: self.evicted.load(Ordering::Relaxed),
                rtty: self.rtty.load(Ordering::Relaxed),
                not_cw: self.not_cw.load(Ordering::Relaxed),
                unkeyed: self.unkeyed.load(Ordering::Relaxed),
            },
            gate_records_total: self.gate_records_total.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn publish_counts(&self, closes: CloseCounts, gate_records_total: u64) {
        self.unconfirmed
            .store(closes.unconfirmed, Ordering::Relaxed);
        self.hang_expired
            .store(closes.hang_expired, Ordering::Relaxed);
        self.silent.store(closes.silent, Ordering::Relaxed);
        self.merged.store(closes.merged, Ordering::Relaxed);
        self.evicted.store(closes.evicted, Ordering::Relaxed);
        self.rtty.store(closes.rtty, Ordering::Relaxed);
        self.not_cw.store(closes.not_cw, Ordering::Relaxed);
        self.unkeyed.store(closes.unkeyed, Ordering::Relaxed);
        self.gate_records_total
            .store(gate_records_total, Ordering::Relaxed);
    }

    /// Publishes the gauge and peak, then advances the steady-state chunk count.
    pub(crate) fn publish_chunk(&self, active_tracks: usize) {
        self.active_tracks
            .store(active_tracks as u64, Ordering::Relaxed);
        self.observe_peak(active_tracks);
        self.chunks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn observe_peak(&self, active_tracks: usize) {
        self.peak_active_tracks
            .fetch_max(active_tracks as u64, Ordering::Relaxed);
    }

    pub(crate) fn clear_active(&self) {
        self.active_tracks.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_preserves_counts_and_peak_when_the_gauge_clears() {
        let obs = TrackLifecycleObserver::default();
        assert_eq!(obs.snapshot(), TrackLifecycleSnapshot::default());
        let closes = CloseCounts {
            unconfirmed: 1,
            hang_expired: 2,
            silent: 3,
            merged: 4,
            evicted: 5,
            rtty: 6,
            not_cw: 7,
            unkeyed: 8,
        };
        obs.publish_counts(closes, 19);
        obs.publish_chunk(3);
        obs.publish_chunk(2);
        assert_eq!(obs.snapshot().peak_active_tracks, 3);
        obs.observe_peak(5);
        obs.observe_peak(1);
        assert_eq!(
            obs.snapshot(),
            TrackLifecycleSnapshot {
                chunks: 2,
                active_tracks: 2,
                peak_active_tracks: 5,
                close_counts: closes,
                gate_records_total: 19,
            }
        );
        obs.clear_active();
        assert_eq!(
            obs.snapshot(),
            TrackLifecycleSnapshot {
                chunks: 2,
                active_tracks: 0,
                peak_active_tracks: 5,
                close_counts: closes,
                gate_records_total: 19,
            }
        );
    }
}
