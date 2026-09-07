//! MAN-100: per-track ledger of decoded, spottable-shaped words, and the
//! cross-candidate arbitration query built on it. ARCHITECTURE §6 step 4b.
//!
//! Purely subtractive: this structure can only ever *withhold* a spot the
//! rest of the pipeline would have emitted. It never enables one, so it
//! cannot create a false spot on its own -- deliberate, given that false
//! spots are what de-lists an RBN node.
//!
//! `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule 2).
//! `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly into
//! whether a `Spot` is emitted, so its iteration order is output-affecting.

use crate::gate::{self, MIN_MESSAGE_TIME_GAP_SECONDS, WINDOW_SECONDS};
use crate::variant::{self, Relation};
use std::collections::BTreeMap;

/// A shape-only override (`longer_containment` below) must still clear
/// the same minimum standalone support any spottable candidate itself
/// must clear (`validator.rs`'s own `reps < 2` gate) -- otherwise a
/// single stray, garbled decode that happens to be a textual
/// prefix-extension of a well-supported genuine call could permanently
/// veto it (MAN-100 remediation C5: measured, a lone "K5ARHT" glued-tail
/// artifact suppressed a 3-rep "K5ARH" outright, with nothing spotted in
/// its place). The measured V8/V8w truncation cases this override exists
/// for all had multi-rep rivals, so this floor costs nothing on real
/// data while closing the single-stray-decode failure mode.
const MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE: u32 = 2;

/// One observed decode of a plausible-shaped word on a track.
struct Obs {
    word_seq: u64,
    sample_ts: u64,
    /// Geometric mean of the word's per-character confidences (the same
    /// quantity `confidence::c_call` computes before its repetition
    /// factor) -- see `confidence::geo_mean`.
    geo_conf: f32,
}

/// How much evidence a candidate has accumulated on a track: message-
/// distinct repetitions (counted the same way as `RepetitionGate`, see
/// `gate::count_message_distinct`), and their summed per-occurrence
/// confidence, used only to break ties on `reps`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Support {
    pub reps: u32,
    pub conf_sum: f32,
}

impl Support {
    /// More message-distinct repetitions wins; equal reps fall back to
    /// summed per-occurrence confidence (the measured "W4KTNL 2 reps/0.885
    /// loses to W4KCL 2 reps/1.016" shape).
    pub fn strictly_better_than(&self, other: &Support) -> bool {
        if self.reps != other.reps {
            self.reps > other.reps
        } else {
            self.conf_sum > other.conf_sum
        }
    }
}

pub struct SupportLedger {
    window_samples: u64,
    time_gap_samples: u64,
    seen: BTreeMap<(u32, String), Vec<Obs>>,
}

impl SupportLedger {
    pub fn new(fs: f64) -> Self {
        Self {
            window_samples: (WINDOW_SECONDS * fs) as u64,
            time_gap_samples: (MIN_MESSAGE_TIME_GAP_SECONDS * fs) as u64,
            seen: BTreeMap::new(),
        }
    }

    /// Records one decode of a plausible-shaped word (`grammar::is_plausible`
    /// AND `cty::is_allocated` -- a form that could never be spotted must
    /// not be able to veto one that could) on `track_id` at `sample_ts`.
    pub fn observe(
        &mut self,
        track_id: u32,
        text: &str,
        word_seq: u64,
        sample_ts: u64,
        geo_conf: f32,
    ) {
        let entry = self.seen.entry((track_id, text.to_string())).or_default();
        entry.push(Obs {
            word_seq,
            sample_ts,
            geo_conf,
        });
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        entry.retain(|o| o.sample_ts >= cutoff);
        self.evict_aged_out(track_id, cutoff);
    }

    /// Drops every `(track_id, *)` entry whose newest observation has
    /// aged out of the window (MAN-100 remediation C6). Without this,
    /// `seen` grows one entry per distinct plausible-shaped garble ever
    /// decoded on a long-lived track: `observe`'s own `retain` only
    /// prunes the ONE entry it just touched, so a text never observed
    /// again keeps its last (now-stale) observations, and its key,
    /// forever -- `forget_track` only helps once the whole track closes,
    /// which the 24h-soak shape MAN-19 exists for never does mid-run.
    /// Swept on every `observe` call rather than lazily, so both the
    /// per-track key count and the O(keys) cost `better_supported_rival`
    /// pays per candidate evaluation stay bounded by what's live in the
    /// window, not by track history.
    fn evict_aged_out(&mut self, track_id: u32, cutoff: u64) {
        let stale: Vec<(u32, String)> = self
            .seen
            .range((track_id, String::new())..)
            .take_while(|(k, _)| k.0 == track_id)
            .filter(|(_, obs)| obs.iter().all(|o| o.sample_ts < cutoff))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.seen.remove(&k);
        }
    }

    /// Folds `obs` into `Support` by calling the SAME shared greedy
    /// message-gap helper as `RepetitionGate` (`gate::message_distinct_indices`,
    /// word_seq gap OR sample_ts gap -- MAN-100 remediation C2) and, critically,
    /// summing `geo_conf` only for the occurrences that helper actually
    /// returns (the first of each message), not every raw observation.
    /// Confidence must be scoped to the same message-distinct occurrences
    /// `reps` reflects: summing every raw repeat would let a candidate
    /// whose fading corruption happens to repeat verbatim several times
    /// WITHIN one message accumulate a higher `conf_sum` than a genuinely
    /// better-supported rival at equal reps, exactly inverting the
    /// tie-break this exists for (caught by an end-to-end run against the
    /// real V8w fixture during development -- summing every raw
    /// observation left one bogus call, W4KTNL, winning its tie against
    /// W4KCL on raw occurrence count alone). Calling the shared helper
    /// (rather than re-implementing the greedy loop, as this used to)
    /// also guarantees this can never drift from `RepetitionGate`'s own
    /// counting rule (MAN-100 remediation C4).
    fn support_in_window(
        obs: &[Obs],
        now: u64,
        window_samples: u64,
        time_gap_samples: u64,
    ) -> Support {
        let cutoff = now.saturating_sub(window_samples);
        let windowed: Vec<&Obs> = obs.iter().filter(|o| o.sample_ts >= cutoff).collect();
        let occurrences: Vec<(u64, u64)> =
            windowed.iter().map(|o| (o.word_seq, o.sample_ts)).collect();
        let counted = gate::message_distinct_indices(&occurrences, time_gap_samples);
        let reps = counted.len() as u32;
        let conf_sum = counted.iter().map(|&i| windowed[i].geo_conf).sum();
        Support { reps, conf_sum }
    }

    /// Support for `text` on `track_id` as of `now` (a `sample_ts`).
    /// `Support::default()` (0 reps, 0.0 confidence) for a track/text pair
    /// never observed.
    pub fn support(&self, track_id: u32, text: &str, now: u64) -> Support {
        match self.seen.get(&(track_id, text.to_string())) {
            Some(obs) => {
                Self::support_in_window(obs, now, self.window_samples, self.time_gap_samples)
            }
            None => Support::default(),
        }
    }

    /// The best confusable rival to `candidate` observed on `track_id`
    /// that beats it, if any -- the cross-candidate arbitration query
    /// (MAN-100 Scenario 1). Compares against every OTHER observed,
    /// plausible-shaped word on the same track inside the same window,
    /// not against already-emitted spots: a genuine rival can win this
    /// comparison well before it has itself been spotted.
    pub fn better_supported_rival(
        &self,
        track_id: u32,
        candidate: &str,
        now: u64,
    ) -> Option<(String, Support)> {
        let mine = self.support(track_id, candidate, now);
        let mut best: Option<(String, Support)> = None;
        for ((tid, text), obs) in self.seen.range((track_id, String::new())..) {
            if *tid != track_id {
                break;
            }
            if text == candidate {
                continue;
            }
            let Some(rel) = variant::relation(candidate, text) else {
                continue;
            };
            let s = Self::support_in_window(obs, now, self.window_samples, self.time_gap_samples);
            if s.reps == 0 {
                continue;
            }
            // Prefix containment is decided by shape, not by support:
            // across two measured 50-signal pileup scenes, a plausible
            // decoded word that is a strict prefix of the track's true
            // call occurred 25 times, and a word of which the true call is
            // a strict prefix occurred 0 times -- truncation is the
            // failure mode, a tail merge is not. The *head*-merge case
            // ("DE" glued onto a call) makes the true call a strict
            // SUFFIX of the artifact, which is why this arm is
            // prefix-only: it must never fire in the other direction, or
            // a genuine call would lose to a merge artifact that happened
            // to decode first (see `variant::relation`'s docs and the
            // MAN-100 decision record for the measured 25:0 split). Still
            // gated on `MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE`, though: shape
            // alone is trusted to override a support comparison, but not
            // to override the same >= 2-rep floor every other spottable
            // candidate must itself clear (MAN-100 remediation C5).
            let longer_containment = rel == Relation::Containment
                && text.len() > candidate.len()
                && text.starts_with(candidate)
                && s.reps >= MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE;
            // The reverse must also be shape-decided, not support-decided
            // (MAN-100 remediation C1): when the CANDIDATE is the longer
            // form and `text` is a strict prefix of it, `text` is the
            // truncation artifact and must never be allowed to win this
            // comparison, however many reps it has racked up. Without
            // this, a truncation that simply arrives first and reaches 2
            // reps before the real call has any could suppress the real
            // call forever after (measured: "CQ DE W6JQ K" x3 then "CQ DE
            // W6JQA K" x2 on one track spotted only the truncation).
            // Prefix-only for the same reason `longer_containment` is: a
            // head-merge rival (`DEN3NXI` vs `N3NXI`) is a SUFFIX
            // relationship, not a prefix one, so it's untouched by this
            // arm and the ordinary support comparison still decides it.
            let shorter_prefix_of_candidate = rel == Relation::Containment
                && candidate.len() > text.len()
                && candidate.starts_with(text.as_str());
            if shorter_prefix_of_candidate {
                continue;
            }
            if !longer_containment && !s.strictly_better_than(&mine) {
                continue;
            }
            let better = match &best {
                None => true,
                Some((_, b)) => s.strictly_better_than(b),
            };
            if better {
                best = Some((text.clone(), s));
            }
        }
        best
    }

    /// Drops every recorded observation for `track_id`. Mirrors
    /// `RepetitionGate::forget_track` (MAN-19) -- without this, `seen`'s
    /// key space grows forever under sustained track churn.
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
    fn an_unobserved_call_has_no_support() {
        let ledger = SupportLedger::new(FS);
        assert_eq!(ledger.support(1, "K5ARH", 0), Support::default());
    }

    #[test]
    fn adjacent_observations_are_one_message_of_support() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 4, 0, 1.0);
        ledger.observe(1, "K5ARH", 5, 10_000, 1.0);
        assert_eq!(ledger.support(1, "K5ARH", 10_000).reps, 1);
    }

    #[test]
    fn observations_three_words_apart_are_two_messages() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 4, 0, 1.0);
        ledger.observe(1, "K5ARH", 7, 10_000, 1.0);
        assert_eq!(ledger.support(1, "K5ARH", 10_000).reps, 2);
    }

    #[test]
    fn observations_outside_the_90s_window_do_not_count() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 0, 0, 1.0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(
            ledger.support(1, "K5ARH", window_samples + 1).reps,
            0,
            "an observation must age out of the same window the gate uses"
        );
    }

    /// The measured track-86 shape: W4KTNL 2 reps/0.885 loses to W4KCL 2
    /// reps/1.016 -- a plain "more reps wins" rule can't separate them, so
    /// the summed-confidence tiebreak decides.
    #[test]
    fn ties_on_reps_are_broken_by_summed_confidence() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "W4KTNL", 9, 0, 0.4);
        ledger.observe(1, "W4KTNL", 22, 10_000, 0.485);
        ledger.observe(1, "W4KCL", 0, 0, 0.5);
        ledger.observe(1, "W4KCL", 10, 10_000, 0.516);

        let rival = ledger
            .better_supported_rival(1, "W4KTNL", 10_000)
            .expect("W4KCL must beat W4KTNL on tied reps via summed confidence");
        assert_eq!(rival.0, "W4KCL");
    }

    /// The measured track-90 shape: W6JQ 3 reps beats W6JQA 2 reps on
    /// plain support, but W6JQ is a strict prefix of W6JQA -- the
    /// containment asymmetry must still let the longer form win. W6JQA
    /// carries 2 (not 1) reps here specifically to also clear
    /// `MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE` (MAN-100 remediation C5) --
    /// see `a_lone_stray_decode_does_not_veto_a_well_supported_candidate`
    /// for the case where it doesn't.
    #[test]
    fn a_strict_prefix_loses_to_a_longer_form_even_with_more_reps() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "W6JQ", 12, 0, 0.3);
        ledger.observe(1, "W6JQ", 19, 10_000, 0.3);
        ledger.observe(1, "W6JQ", 47, 20_000, 0.3);
        ledger.observe(1, "W6JQA", 30, 15_000, 0.3);
        ledger.observe(1, "W6JQA", 40, 18_000, 0.3);

        let rival = ledger
            .better_supported_rival(1, "W6JQ", 20_000)
            .expect("W6JQA must beat W6JQ via the prefix-containment asymmetry");
        assert_eq!(rival.0, "W6JQA");
    }

    /// MAN-100 remediation C5: a lone, 1-rep stray decode that happens to
    /// be a textual prefix-extension of a well-supported genuine call
    /// must NOT veto it -- shape alone is not enough; the rival must also
    /// clear `MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE`. Measured: a single
    /// garbled "K5ARHT" (a trailing "T" glued onto the real call)
    /// suppressed a 3-rep "K5ARH" outright, with nothing spotted in its
    /// place.
    #[test]
    fn a_lone_stray_decode_does_not_veto_a_well_supported_candidate() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 4, 0, 0.3);
        ledger.observe(1, "K5ARHT", 6, 5_000, 0.3);
        ledger.observe(1, "K5ARH", 8, 10_000, 0.3);

        assert!(
            ledger.better_supported_rival(1, "K5ARH", 10_000).is_none(),
            "a lone 1-rep glued-tail artifact must not veto a well- \
             supported (>= 2 message-distinct reps) genuine call"
        );
    }

    /// MAN-100 remediation C1: the truncation-arrives-first ordering. The
    /// truncation reaches 2 reps (enough to itself beat `Support::default`)
    /// before the longer, genuine form has any support at all -- a pure
    /// support comparison for the LONGER form's own arbitration call would
    /// let the truncation win. The prefix-containment asymmetry must fire
    /// in this direction too, not just when arbitrating the shorter form.
    #[test]
    fn a_truncation_that_arrives_first_still_loses_to_the_longer_form() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "W6JQ", 4, 0, 0.3);
        ledger.observe(1, "W6JQ", 10, 10_000, 0.3);
        ledger.observe(1, "W6JQA", 20, 20_000, 0.3);

        assert!(
            ledger.better_supported_rival(1, "W6JQA", 20_000).is_none(),
            "a 2-rep truncation that arrived first must not suppress the \
             longer, genuine form once it appears, even though the \
             truncation's rep count is still ahead (2 vs the genuine \
             form's 1)"
        );
    }

    /// The measured V8 shape: N3NXI 6 reps must beat the DEN3NXI 1-rep
    /// merge artifact. N3NXI is a strict SUFFIX of DEN3NXI (a framing "DE"
    /// glued onto the call), so the containment asymmetry must NOT fire in
    /// this direction -- only the ordinary support comparison applies, and
    /// N3NXI's 6 reps beats DEN3NXI's 1.
    #[test]
    fn a_strict_suffix_does_not_lose_to_a_longer_form_on_length_alone() {
        let mut ledger = SupportLedger::new(FS);
        for i in 0..6u64 {
            ledger.observe(1, "N3NXI", i * 4, i * 5_000, 0.3);
        }
        ledger.observe(1, "DEN3NXI", 2, 5_000, 0.3);

        assert!(
            ledger.better_supported_rival(1, "N3NXI", 25_000).is_none(),
            "the well-supported real call must not lose to a 1-rep merge \
             artifact that happens to be textually longer"
        );
    }

    /// The flip side: the 1-rep merge artifact itself must lose to the
    /// well-supported real call it was glued onto.
    #[test]
    fn a_head_merge_artifact_loses_to_the_well_supported_real_call() {
        let mut ledger = SupportLedger::new(FS);
        for i in 0..6u64 {
            ledger.observe(1, "N3NXI", i * 4, i * 5_000, 0.3);
        }
        ledger.observe(1, "DEN3NXI", 2, 5_000, 0.3);

        let rival = ledger
            .better_supported_rival(1, "DEN3NXI", 25_000)
            .expect("DEN3NXI must lose to the far-better-supported N3NXI");
        assert_eq!(rival.0, "N3NXI");
    }

    #[test]
    fn unrelated_candidates_on_the_same_track_never_arbitrate_against_each_other() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 0, 0, 0.5);
        ledger.observe(1, "K5ARH", 10, 10_000, 0.5);
        ledger.observe(1, "W1AW", 0, 0, 0.9);
        ledger.observe(1, "W1AW", 10, 10_000, 0.9);
        assert!(ledger.better_supported_rival(1, "K5ARH", 10_000).is_none());
        assert!(ledger.better_supported_rival(1, "W1AW", 10_000).is_none());
    }

    #[test]
    fn forget_track_drops_every_entry_for_that_track_only() {
        let mut ledger = SupportLedger::new(FS);
        ledger.observe(1, "K5ARH", 0, 0, 0.5);
        ledger.observe(2, "K5ARH", 0, 0, 0.5);
        ledger.forget_track(1);
        assert_eq!(ledger.support(1, "K5ARH", 0), Support::default());
        assert_eq!(ledger.support(2, "K5ARH", 0).reps, 1);
    }

    #[test]
    fn sustained_track_churn_stays_bounded() {
        let mut ledger = SupportLedger::new(FS);
        for track_id in 0..10_000u32 {
            ledger.observe(track_id, "K5ARH", 0, 0, 0.5);
            ledger.forget_track(track_id);
        }
        assert_eq!(
            ledger.seen.len(),
            0,
            "seen must not accumulate one entry per historical track_id"
        );
    }

    /// MAN-100 remediation C6: reproduces the 24h-soak shape MAN-19 exists
    /// for, but WITHOUT track churn -- a single, long-lived track that
    /// keeps decoding new, never-repeated garbled words well outside each
    /// other's 90s window. `forget_track` (MAN-19's fix) never fires here
    /// since the track never closes; only `observe`'s own aging-out
    /// eviction can bound growth.
    #[test]
    fn a_single_long_lived_track_stays_bounded_as_its_garbles_age_out() {
        let mut ledger = SupportLedger::new(FS);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        for i in 0..10_000u64 {
            let text = format!("K{i}AB");
            ledger.observe(1, &text, i, i * (window_samples + 1), 0.5);
        }
        assert_eq!(
            ledger.seen.len(),
            1,
            "seen must not accumulate one entry per historical garble on a \
             single long-lived track once each ages out of the window"
        );
    }
}
