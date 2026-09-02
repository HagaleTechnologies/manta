//! Repetition gate: a callsign must decode >= 2 distinct times within a
//! 90 s window on its track before first spot. SPEC §4.6 / ARCHITECTURE
//! §6.4. `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule
//! 2). `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly
//! into whether/when a `Spot` is emitted.

use std::collections::BTreeMap;

const WINDOW_SECONDS: f64 = 90.0;

pub struct RepetitionGate {
    window_samples: u64,
    seen: BTreeMap<(u32, String), Vec<u64>>,
}

impl RepetitionGate {
    pub fn new(fs: f64) -> Self {
        Self {
            window_samples: (WINDOW_SECONDS * fs) as u64,
            seen: BTreeMap::new(),
        }
    }

    /// Records one decode of `callsign` on `track_id` at `sample_ts`.
    /// Returns the number of distinct decodes within the trailing window
    /// (including this one).
    pub fn record(&mut self, track_id: u32, callsign: &str, sample_ts: u64) -> usize {
        let entry = self
            .seen
            .entry((track_id, callsign.to_string()))
            .or_default();
        entry.push(sample_ts);
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        entry.retain(|&ts| ts >= cutoff);
        entry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_decode_counts_as_one() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, "K5ARH", 0), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        assert_eq!(gate.record(1, "K5ARH", 100_000), 2);
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(gate.record(1, "K5ARH", window_samples + 1), 1);
    }

    #[test]
    fn different_tracks_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        assert_eq!(gate.record(2, "K5ARH", 0), 1);
        assert_eq!(gate.record(1, "W1AW", 0), 1);
    }
}
