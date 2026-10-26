//! Re-spot suppression. ARCHITECTURE §6.5. `sample_ts`-based (SPEC
//! -decode-core.md §6 rule 2). `BTreeMap`, never `HashMap` (rule 3).
//!
//! MAN-13 (D6): with several inputs feeding one validator, a report from a
//! **different input** than the stored spot's is a duplicate for the whole
//! suppression window when it falls in the same 300 Hz bucket, or within
//! one channel ([`CROSS_INPUT_MAX_HZ`]) of the stored spot across a bucket
//! edge. No SNR or spot-type override applies across inputs: two receivers
//! hearing one transmission must not spot it twice just because one antenna
//! hears it 6 dB louder. The stored timestamp never moves backwards: an
//! override emitted with an older `sample_ts` (another input's batch merged
//! late) keeps the newer one.
//!
//! MAN-120: a report from the stored spot's **own input** that lands in
//! the neighbouring bucket within one channel of the stored spot is the
//! same station straddling a 300 Hz bucket edge, and is withheld as a
//! repeat (same-input SNR/type overrides still apply) -- the same
//! Hz-bounded neighbour join the repetition gate adopted in MAN-175
//! (`gate.rs`), never bare bucket adjacency (a bare join reaches ~200 Hz
//! apart). This supersedes the same-input-unchanged sliver of MAN-13's
//! D6; the wider tolerance-versus-bucket question for all spots stays
//! with MAN-16
//! (docs/DECISIONS/2026-09-03-cross-source-dedup-algorithm-spike.md).
//!
//! MAN-436: those same-input overrides are decided against the newer of
//! the two spots (this bucket's and the joined one across the edge), so a
//! station wobbling `Cq -> De -> Cq` across the edge re-emits as it does
//! at one frequency.

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

/// MAN-14: what `Dedupe::assess` decided for one candidate spot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupeOutcome {
    /// Emit it (and it is now the stored spot).
    Emit,
    /// The same input already reported it inside the window, with no SNR
    /// improvement or type change.
    WithheldRepeat,
    /// A different input already reported the same signal inside the
    /// window (MAN-13 D6): the reporting input's cross-input duplicate.
    WithheldCrossInput,
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
    ///
    /// MAN-120: the identity includes `input`'s own spot one bucket over
    /// within one channel (`assess`'s same-input edge join), so the peek
    /// returns the newer of the two stored timestamps -- a newer spot of
    /// the same station across the edge makes the deferred candidate
    /// stale too (Codex review on PR #387).
    pub(crate) fn last_sample_ts(&self, callsign: &str, freq_hz: f64, input: u32) -> Option<u64> {
        let bucket = Self::bucket(freq_hz);
        let own = self
            .last
            .get(&(callsign.to_string(), bucket))
            .map(|s| s.sample_ts);
        let across_edge = self
            .last
            .get(&(callsign.to_string(), Self::neighbour(freq_hz, bucket)))
            .filter(|prev| {
                prev.input == input && (freq_hz - prev.freq_hz).abs() < CROSS_INPUT_MAX_HZ
            })
            .map(|s| s.sample_ts);
        own.max(across_edge)
    }

    /// The bucket across the 300 Hz edge nearer to `freq_hz`.
    fn neighbour(freq_hz: f64, bucket: i64) -> i64 {
        if freq_hz / FREQ_BUCKET_HZ >= bucket as f64 {
            bucket + 1
        } else {
            bucket - 1
        }
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
        matches!(
            self.assess(callsign, freq_hz, snr_db, spot_type, sample_ts, input),
            DedupeOutcome::Emit
        )
    }

    /// `should_emit`, saying why a withheld spot was withheld (MAN-14): a
    /// different input's in-window spot, in this bucket or across the edge,
    /// is `WithheldCrossInput`; this input's own in-window repeat is
    /// `WithheldRepeat`. Records the spot exactly when it returns `Emit`.
    pub fn assess(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        snr_db: f32,
        spot_type: SpotType,
        sample_ts: u64,
        input: u32,
    ) -> DedupeOutcome {
        let bucket = Self::bucket(freq_hz);
        let key = (callsign.to_string(), bucket);
        let window = self.suppression_window_samples;
        let in_window = |prev: &LastSpot| sample_ts.saturating_sub(prev.sample_ts) < window;
        // The neighbouring bucket's in-window spot within one channel is
        // the same signal straddling the edge -- whatever this bucket holds
        // (nothing, an expired spot, or this input's own). From another
        // input it is a cross-input duplicate (MAN-13); from this input it
        // is this station's spot (MAN-120's join). Hz-bounded like the
        // gate's MAN-175 anchor join, never bare bucket adjacency (which
        // reaches ~200 Hz apart, `gate.rs`'s recorded incident).
        let neighbour = Self::neighbour(freq_hz, bucket);
        let joined = self
            .last
            .get(&(callsign.to_string(), neighbour))
            .filter(|prev| (freq_hz - prev.freq_hz).abs() < CROSS_INPUT_MAX_HZ && in_window(prev));
        let own = self.last.get(&key);
        let outcome = if joined.is_some_and(|prev| prev.input != input)
            || own.is_some_and(|prev| prev.input != input && in_window(prev))
        {
            DedupeOutcome::WithheldCrossInput
        } else {
            // MAN-436: decide against this station's newest spot, on
            // either side of the edge, as one frequency decides against
            // the last spot it stored. A stale spot on the other side
            // neither withholds a type change against the newer one nor
            // stands in for it. Ties keep the own-bucket spot. A withheld
            // report stores nothing, so every stored entry and
            // `last_sample_ts` are untouched by construction.
            let own = own.filter(|prev| prev.input == input);
            let edge = joined.filter(|prev| prev.input == input);
            let latest = match (own, edge) {
                (Some(o), Some(e)) if e.sample_ts > o.sample_ts => Some(e),
                (o, e) => o.or(e),
            };
            if latest.is_none_or(|prev| {
                !in_window(prev)
                    || snr_db - prev.snr_db >= SNR_IMPROVEMENT_DB
                    || spot_type != prev.spot_type
            }) {
                DedupeOutcome::Emit
            } else {
                DedupeOutcome::WithheldRepeat
            }
        };
        if outcome == DedupeOutcome::Emit {
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
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    /// Review finding: the cross-edge match must not depend on this
    /// bucket being empty. Input 0's own bucket holds an expired spot;
    /// input 1 has just spotted the same signal 20 Hz away across the
    /// 300 Hz edge at 14_012_550 Hz.
    #[test]
    fn another_inputs_report_across_a_bucket_edge_is_a_duplicate_over_an_expired_entry() {
        let mut d = Dedupe::new(FS);
        let window = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit("W1AW", 14_012_540.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_560.0, 20.0, SpotType::Cq, window + 1000, 1));
        assert!(!d.should_emit("W1AW", 14_012_540.0, 20.0, SpotType::Cq, window + 2000, 0));
        // One input alone: an *expired* cross-edge spot re-emits; an
        // in-window one is a repeat (MAN-120).
        let mut one = Dedupe::new(FS);
        assert!(one.should_emit("W1AW", 14_012_540.0, 20.0, SpotType::Cq, 0, 0));
        assert!(one.should_emit("W1AW", 14_012_560.0, 20.0, SpotType::Cq, window + 1000, 0));
        assert!(!one.should_emit("W1AW", 14_012_540.0, 20.0, SpotType::Cq, window + 2000, 0));
    }

    // MAN-120: same-input straddle across a 300 Hz bucket edge.

    /// The ticket's gherkin scenario, pinned: a station whose frequency
    /// sits on a 300 Hz bucket boundary and drifts 2 Hz across it between
    /// spots is one station. bucket(14_012_549) == 46_708 and
    /// bucket(14_012_551) == 46_709, so the drift crosses the edge at
    /// 14_012_550 Hz. Reproduced failing at HEAD by the MAN-120 plan
    /// phase: the second report emitted ("DOUBLE-SPOTTED: same input,
    /// same station, 2 Hz drift across a bucket edge").
    #[test]
    fn same_input_report_across_a_bucket_edge_within_one_channel_is_a_repeat() {
        let mut d = Dedupe::new(FS);
        assert_eq!(Dedupe::bucket(14_012_549.0), 46_708);
        assert_eq!(Dedupe::bucket(14_012_551.0), 46_709);
        assert_eq!(
            d.assess("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 0, 0),
            DedupeOutcome::Emit
        );
        assert_eq!(
            d.assess("W1AW", 14_012_551.0, 20.0, SpotType::Cq, FS as u64, 0),
            DedupeOutcome::WithheldRepeat
        );
    }

    /// The same-input overrides travel with the join: a +6 dB SNR jump
    /// across the edge emits (and stores -- the follow-up without the
    /// override is then an own-key repeat).
    #[test]
    fn same_input_snr_jump_across_the_edge_still_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_551.0, 26.0, SpotType::Cq, FS as u64, 0));
        assert!(!d.should_emit("W1AW", 14_012_551.0, 26.0, SpotType::Cq, 2 * FS as u64, 0));
    }

    /// ... and so does a spot-type change.
    #[test]
    fn same_input_type_change_across_the_edge_still_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_551.0, 20.0, SpotType::De, FS as u64, 0));
        assert!(!d.should_emit("W1AW", 14_012_551.0, 20.0, SpotType::De, 2 * FS as u64, 0));
    }

    /// Hz-bounded, not bare adjacency (gate.rs's MAN-175 lesson: a bare
    /// ±1-bucket join reaches ~200 Hz apart): 14_012_500 and 14_012_600
    /// round to adjacent buckets but sit 100 Hz apart -- more than one
    /// channel -- so they are two different stations.
    #[test]
    fn same_input_across_edge_more_than_one_channel_away_emits() {
        let mut d = Dedupe::new(FS);
        assert_ne!(Dedupe::bucket(14_012_500.0), Dedupe::bucket(14_012_600.0));
        assert!(d.should_emit("W1AW", 14_012_500.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_600.0, 20.0, SpotType::Cq, FS as u64, 0));
    }

    /// An expired neighbour-bucket spot no longer joins: the window
    /// elapsed, so this is a fresh transmission.
    #[test]
    fn same_input_across_edge_after_the_window_elapses_emits() {
        let mut d = Dedupe::new(FS);
        let window = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 0, 0));
        assert!(d.should_emit("W1AW", 14_012_551.0, 20.0, SpotType::Cq, window + 1000, 0));
    }

    /// Wobbling back and forth across the edge is withheld in both
    /// directions: the cross-edge report via the neighbour join, the
    /// return via the own-key entry it never overwrote.
    #[test]
    fn straddle_then_drift_back_is_withheld_both_ways() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 0, 0));
        assert!(!d.should_emit("W1AW", 14_012_551.0, 20.0, SpotType::Cq, FS as u64, 0));
        assert!(!d.should_emit("W1AW", 14_012_549.0, 20.0, SpotType::Cq, 2 * FS as u64, 0));
    }

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
        assert_eq!(d.last_sample_ts("W1AW", 14_012_340.0, 0), Some(50_000));
    }

    /// MAN-14: `assess` names the withhold reason.
    #[test]
    fn assess_classifies_each_withhold_reason() {
        use DedupeOutcome::*;
        let window = (SUPPRESSION_SECONDS * FS) as u64;
        // Another input, same bucket, in window.
        let mut d = Dedupe::new(FS);
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 0, 0),
            Emit
        );
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 30.0, SpotType::De, 1000, 1),
            WithheldCrossInput
        );
        // This input's own repeat.
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 20.0, SpotType::Cq, 2000, 0),
            WithheldRepeat
        );
        // SNR improvement and a type change on the same input emit.
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 26.0, SpotType::Cq, 3000, 0),
            Emit
        );
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 26.0, SpotType::De, 4000, 0),
            Emit
        );
        // After the window, another input emits.
        assert_eq!(
            d.assess("W1AW", 14_012_340.0, 20.0, SpotType::De, 4000 + window, 1),
            Emit
        );
        // Another input across a bucket edge, within one channel.
        let mut e = Dedupe::new(FS);
        assert_eq!(
            e.assess("W1AW", 14_012_540.0, 20.0, SpotType::Cq, 0, 0),
            Emit
        );
        assert_eq!(
            e.assess("W1AW", 14_012_560.0, 20.0, SpotType::Cq, 1000, 1),
            WithheldCrossInput
        );
    }

    /// MAN-436: a station wobbling across the edge decides each report as
    /// one frequency would -- against its newest spot, on either side.
    /// Reproduced at HEAD: the ticket's `Cq(L) De(R) Cq(L)`, its mirror,
    /// and `Cq(R) De(L) Cq(L)` each withheld the third report.
    #[test]
    fn an_edge_wobble_decides_against_the_newest_spot_as_one_frequency_does() {
        use DedupeOutcome::{Emit, WithheldRepeat};
        use SpotType::{Cq, De};
        type Step = (f64, f32, SpotType);
        let (l, r) = (14_012_549.0, 14_012_551.0);
        let run = |steps: &[Step], one_freq: bool| -> Vec<DedupeOutcome> {
            let mut d = Dedupe::new(FS);
            (0u64..)
                .zip(steps)
                .map(|(i, &(f, snr, ty))| {
                    let f = if one_freq { l } else { f };
                    d.assess("W1AW", f, snr, ty, i * FS as u64, 0)
                })
                .collect()
        };
        let cases: [(&[Step], &[DedupeOutcome]); 6] = [
            (
                &[(l, 20.0, Cq), (r, 20.0, De), (l, 20.0, Cq)],
                &[Emit, Emit, Emit],
            ),
            (
                &[(r, 20.0, De), (l, 20.0, Cq), (r, 20.0, De)],
                &[Emit, Emit, Emit],
            ),
            (
                &[(r, 20.0, Cq), (l, 20.0, De), (l, 20.0, Cq)],
                &[Emit, Emit, Emit],
            ),
            (
                &[(l, 20.0, Cq), (r, 20.0, De), (l, 20.0, Cq), (r, 20.0, De)],
                &[Emit, Emit, Emit, Emit],
            ),
            (
                &[(l, 20.0, Cq), (r, 20.0, De), (l, 20.0, De)],
                &[Emit, Emit, WithheldRepeat],
            ),
            (
                &[(l, 20.0, Cq), (r, 26.0, Cq), (l, 20.0, Cq)],
                &[Emit, Emit, WithheldRepeat],
            ),
        ];
        for (steps, want) in cases {
            assert_eq!(run(steps, false), want, "across the edge: {steps:?}");
            assert_eq!(run(steps, true), want, "one frequency: {steps:?}");
        }
    }

    /// MAN-14: `should_emit` is `assess == Emit` on the same sequence.
    #[test]
    fn should_emit_agrees_with_assess() {
        let window = (SUPPRESSION_SECONDS * FS) as u64;
        let steps: [(f64, f32, SpotType, u64, u32); 8] = [
            (14_012_340.0, 20.0, SpotType::Cq, 0, 0),
            (14_012_340.0, 20.0, SpotType::Cq, 1000, 1),
            (14_012_340.0, 20.0, SpotType::Cq, 2000, 0),
            (14_012_340.0, 27.0, SpotType::Cq, 3000, 0),
            (14_012_560.0, 20.0, SpotType::Cq, 4000, 1),
            (14_012_540.0, 20.0, SpotType::Cq, 5000, 0),
            (14_012_340.0, 20.0, SpotType::De, 6000, 1),
            (14_012_340.0, 20.0, SpotType::Cq, 6000 + window, 1),
        ];
        let mut a = Dedupe::new(FS);
        let mut b = Dedupe::new(FS);
        for (freq, snr, ty, ts, input) in steps {
            let emitted = a.should_emit("W1AW", freq, snr, ty, ts, input);
            let outcome = b.assess("W1AW", freq, snr, ty, ts, input);
            assert_eq!(
                emitted,
                outcome == DedupeOutcome::Emit,
                "{freq} {ts} {input}"
            );
        }
    }
}
