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

    /// Drops every recorded decode for `track_id`. MAN-19: without this,
    /// `seen`'s key space grows forever under sustained track churn --
    /// `record`'s own `retain` only prunes an entry's *timestamps*, and
    /// only when that same key is recorded again; a track that goes
    /// silent for good leaves its now-stale key sitting in the map with
    /// no further `record` call ever revisiting it to notice. Call once a
    /// track closes (any `CloseReason` -- `DecoderEvent::TrackClosed`).
    pub fn forget_track(&mut self, track_id: u32) {
        let keys: Vec<(u32, String)> = self
            .seen
            .range((track_id, String::new())..)
            .take_while(|(k, _)| k.0 == track_id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.seen.remove(&k);
        }
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

    /// MAN-19: `forget_track` must remove every `(track_id, *)` key --
    /// otherwise `seen` grows one entry per distinct (track_id, callsign)
    /// pair ever recorded, forever, since `track_id`s are never reused and
    /// nothing else ever revisits an abandoned key to prune it.
    #[test]
    fn forget_track_removes_every_entry_for_that_track_only() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        gate.record(1, "W1AW", 0);
        gate.record(2, "K5ARH", 0);
        assert_eq!(gate.seen.len(), 3);

        gate.forget_track(1);
        assert_eq!(gate.seen.len(), 1, "only track 2's entry should remain");
        assert!(gate.seen.contains_key(&(2, "K5ARH".to_string())));

        // A track with no recorded decodes at all is a no-op, not an error.
        gate.forget_track(99);
        assert_eq!(gate.seen.len(), 1);
    }

    /// MAN-19: reproduces the soak's actual failure mode -- many distinct,
    /// never-reused track_ids each recording once and closing. Without
    /// `forget_track` wired in (this test calls it explicitly, the way
    /// `Validator::ingest`'s `TrackClosed` arm does in production), `seen`
    /// would have 10,000 entries here instead of 0.
    #[test]
    fn sustained_track_churn_stays_bounded_when_each_track_is_forgotten_on_close() {
        let mut gate = RepetitionGate::new(FS);
        for track_id in 0..10_000u32 {
            gate.record(track_id, "K5ARH", 0);
            gate.forget_track(track_id);
        }
        assert_eq!(
            gate.seen.len(),
            0,
            "seen must not accumulate one entry per historical track_id"
        );
    }
}
