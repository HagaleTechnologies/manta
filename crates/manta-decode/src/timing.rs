//! Online 2-means speed tracking and gap classification. SPEC §4.1–§4.2.

use std::collections::VecDeque;

const CLUSTER_ALPHA: f32 = 0.15; // SPEC §9 decode.cluster_alpha
const RATIO_MIN: f32 = 2.2; // SPEC §9 decode.mu_ratio_bounds
const RATIO_MAX: f32 = 4.5;
const DIT_CLAMP_MS: (f32, f32) = (20.0, 150.0); // SPEC §4.1 (60..8 WPM)
const WPM_ALPHA: f32 = 0.1; // SPEC §4.1 reporting EMA
const DRIFT_LEN: usize = 12; // SPEC §4.1 regime-change rule
const DRIFT_CV_MAX: f64 = 0.35;
const DRIFT_OFF_FRAC: f64 = 0.40;
// SPEC §9 decode.char_gap_dits. **[DEVIATION]** SPEC §4.2 pins 2.0; lowered
// to 1.6 here -- see docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md.
// Demod's hysteresis+debounce (SPEC §3.3) adds a roughly constant ~15-20ms
// overshoot to every measured mark but not to gap durations, so mu_dit_ms
// (built from marks) runs high relative to true keyed timing. At high WPM
// (short true dit period) that constant overshoot is a large fraction of
// mu_dit, compressing gap_ms/mu_dit_ms ratios enough that real
// inter-character gaps can fall under the nominal 2.0 threshold and get
// merged into the preceding character. A 500-case sweep at 10-40 WPM found
// this misclassifies ~2.2% of multi-character texts at 2.0; 1.6 fixed the
// large majority of those (11 -> 4 failures, reproduced across two
// independent random seeds) with zero cases regressing pass -> fail.
const CHAR_GAP_DITS: f32 = 1.6;
const WORD_GAP_DITS: f32 = 5.0; // SPEC §9 decode.word_gap_dits
const FARNS_LONG_U: f32 = 1.5; // SPEC §4.2 long-gap floor
                               // SPEC §9 decode.min_count nominally pins 8. **[DEVIATION]** lowered to 5,
                               // which is the practical floor for this constant: `ClusterPair::observe`
                               // (below) always needs exactly 5 samples to leave its unimodal `init` phase
                               // and become `ready()` (a fixed threshold shared with `SpeedTracker`'s
                               // mu_dit/mu_dah bootstrap, not specific to Farnsworth), and
                               // `farnsworth_active()` requires both `pair.ready()` and `long_seen >=
                               // FARNS_MIN_COUNT` -- so any value <= 5 is equivalent (confirmed empirically:
                               // 2/3/4/5 all produce identical V10 classification). Values > 5 only add
                               // extra confirmation delay past that floor. This does not fully eliminate
                               // Farnsworth's activation lag -- seeing this shared 5-sample bootstrap
                               // itself takes several inter-character/inter-word gaps on any real
                               // Farnsworth signal, which is why V10's golden test tolerates a small,
                               // documented "warmup" word-boundary count instead of an exact match (see
                               // golden_v7_v9_v10.rs's v10 test and the M2 sub-project 2 close-out pins
                               // doc). Reducing the shared 5-sample bootstrap itself was considered and
                               // rejected for this task: it also drives mark-speed (mu_dit/mu_dah)
                               // estimation for every decode, not just Farnsworth ones, and changing it
                               // needs its own full-suite/multi-WPM validation, out of this task's scope.
const FARNS_MIN_COUNT: u32 = 5;
const FARNS_MIN_RATIO: f32 = 1.8;
// MAN-6 bad-lock recovery. `check_drift`'s regime-change rule requires
// CV < DRIFT_CV_MAX, which makes it structurally blind to a *self-consistent*
// bad lock: when mu_dit is seeded far too low, every real mark -- dits and dahs
// alike -- lands in the `hi` cluster, so there is no "off-centroid" anomaly to
// detect, and the streak's CV is high (~0.47 for a 3-dit/2-dah mixture)
// precisely because the streak is a mixture. That high-CV, single-cluster
// region is unoccupied by the SPEC §4.1 rule, so it is claimed here as an
// explicit recovery trigger, gated on the streak itself splitting into two
// Morse-plausible clusters. See
// docs/DECISIONS/2026-09-04-man6-leading-partial-run-and-badlock-recovery.md.
const BADLOCK_MIN_CLUSTER: usize = 3; // of DRIFT_LEN = 12, per side

fn mean(xs: &[f32]) -> f32 {
    let mut acc = 0.0f64;
    for &x in xs {
        acc += x as f64;
    }
    (acc / xs.len() as f64) as f32
}

/// Index of the largest ratio gap between consecutive values of a
/// (ascending-)sorted slice: `sorted[i+1] / sorted[i]` maximized over `i`.
/// Shared by `ClusterPair::initialize`'s one-shot bimodal split and the
/// MAN-6 bad-lock guard `is_credible_bimodal`, so both agree on exactly
/// where a population splits into two clusters. `sorted` must have at
/// least 2 elements.
fn largest_ratio_gap(sorted: &[f32]) -> usize {
    let mut best_i = 0;
    let mut best_r = 0.0f32;
    for i in 0..sorted.len() - 1 {
        let r = sorted[i + 1] / sorted[i];
        if r > best_r {
            best_r = r;
            best_i = i;
        }
    }
    best_i
}

/// Whether `durs` splits into two clusters that are plausibly dits and
/// dahs: a largest-ratio-gap split with at least `BADLOCK_MIN_CLUSTER`
/// members on each side and a centroid ratio inside SPEC §4.1's
/// `[2.2, 4.5]` window. Used only by the MAN-6 bad-lock check; deliberately
/// stricter than `ClusterPair::initialize`'s own `>= 2.0` gate so that a
/// merely jittery homogeneous run can never be mistaken for a bad lock.
fn is_credible_bimodal(durs: &[f32]) -> bool {
    if durs.len() < 2 * BADLOCK_MIN_CLUSTER {
        return false;
    }
    let mut s = durs.to_vec();
    s.sort_by(f32::total_cmp);
    let best_i = largest_ratio_gap(&s);
    let lo_n = best_i + 1;
    let hi_n = s.len() - lo_n;
    if lo_n < BADLOCK_MIN_CLUSTER || hi_n < BADLOCK_MIN_CLUSTER {
        return false;
    }
    let lo = mean(&s[..lo_n]);
    let hi = mean(&s[lo_n..]);
    if lo <= 0.0 {
        return false;
    }
    (RATIO_MIN..=RATIO_MAX).contains(&(hi / lo))
}

/// Shared 2-means machinery: init-after-5 with largest-ratio-gap split,
/// EMA centroid updates, geometric-mean boundary. SPEC §4.1 (marks) and
/// §4.2 (gaps use "the same 2-means machinery").
#[derive(Debug, Clone)]
struct ClusterPair {
    lo: f32,
    hi: f32,
    init: Vec<f32>,
    ready: bool,
    confirmed: bool,
    /// Unimodal-init fallback direction (pinned decision 20 fix): when the
    /// lone 5-mark cluster's mean already exceeds the SPEC §4.1 dit ceiling,
    /// `hi` holds the real (confirmed-shape) cluster and `lo` is a
    /// provisional placeholder awaiting a genuine dit to re-anchor it --
    /// the mirror of the classic case, where `lo` is real and `hi` is the
    /// placeholder.
    placeholder_is_lo: bool,
    /// Unimodal-init absolute-value ceiling (pinned decision 20 fix, scoped):
    /// `Some(ceiling)` for millisecond-typed callers (SpeedTracker) enables the
    /// "mean > ceiling implies dahs, not dits" prior; `None` for dit-ratio-typed
    /// callers (GapClassifier, whose values are gap_ms/mu_dit_ms, not
    /// milliseconds -- a ceiling comparison there is semantically meaningless
    /// and can misfire on long real-audio silence gaps) preserves the original
    /// "always assume the low cluster is real" default.
    unimodal_ceiling: Option<f32>,
}

impl ClusterPair {
    fn new(unimodal_ceiling: Option<f32>) -> Self {
        ClusterPair {
            lo: 0.0,
            hi: 0.0,
            init: Vec::with_capacity(5),
            ready: false,
            confirmed: false,
            placeholder_is_lo: false,
            unimodal_ceiling,
        }
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn confirmed(&self) -> bool {
        self.confirmed
    }

    fn boundary(&self) -> f32 {
        (self.lo * self.hi).sqrt()
    }

    /// Feed one observation. Returns true while the value was consumed for
    /// initialization (callers exclude those from drift bookkeeping).
    fn observe(&mut self, v: f32) -> bool {
        if !self.ready {
            self.init.push(v);
            if self.init.len() == 5 {
                self.initialize();
            }
            return true;
        }
        if !self.confirmed {
            if self.placeholder_is_lo {
                if v <= 0.5 * self.hi {
                    // Mirror of the dit-assumed re-anchor below: unconfirmed
                    // mu_dit re-anchors to the first genuinely short mark.
                    self.lo = v;
                    self.confirmed = true;
                    return false;
                }
            } else if v >= 2.0 * self.lo {
                // SPEC §4.1: unconfirmed mu_dah re-anchors to the first long mark.
                self.hi = v;
                self.confirmed = true;
                return false;
            }
        }
        if v < self.boundary() {
            self.lo += CLUSTER_ALPHA * (v - self.lo);
        } else {
            self.hi += CLUSTER_ALPHA * (v - self.hi);
        }
        false
    }

    /// Pinned decision 20 (`docs/DECISIONS/2026-07-11-m0-implementation-pins.md`),
    /// fixed here: the unimodal branch below used to always assume the lone
    /// cluster was dits (`mu_dit = mean`, `mu_dah = 3*mean`). A homogeneous
    /// run of dahs (an all-dah opener -- e.g. M, O, or repeated T) then
    /// locked in the wrong scale, because `observe()`'s dit-assumed
    /// re-anchor condition (`v >= 2.0 * self.lo`) can never fire from a
    /// stream of same-length dahs. Fix: an absolute-ms prior using the
    /// existing SPEC §4.1 dit clamp `[20, 150]` ms -- a lone cluster whose
    /// mean already exceeds 150 ms cannot possibly be dits (a real dit is
    /// clamped at 150 ms), so assume dahs instead, with the placeholder
    /// direction flipped (`lo` becomes the provisional guess, `hi` the real
    /// cluster). The ambiguous middle band (roughly 60-150 ms, where either
    /// interpretation is physically plausible depending on operator speed)
    /// still defaults to "assume dits", same as before -- this fix resolves
    /// the unambiguous case the pin's stress sweep exercised, not the
    /// inherently ambiguous one.
    ///
    /// This ceiling is opt-in via `unimodal_ceiling`, not baked into the
    /// branch unconditionally: `ClusterPair` is shared by `SpeedTracker`
    /// (mark durations, milliseconds, where the SPEC §4.1 dit clamp is a
    /// meaningful absolute-value prior) and `GapClassifier` (gap-to-dit
    /// ratios, dimensionless -- see `GapClassifier::classify`'s
    /// `u = gap_ms / mu_dit_ms`). A 150.0 ceiling has no correct semantic
    /// meaning for dit-ratio values; a homogeneous run of long silence gaps
    /// (plausible on live audio) could otherwise trip the "assume dahs"
    /// branch even though GapClassifier's two clusters represent char-gap
    /// vs. word-gap durations, not dit vs. dah. `SpeedTracker` passes
    /// `Some(DIT_CLAMP_MS.1)`; `GapClassifier` passes `None` and always
    /// assumes the low cluster is real, as before this fix existed.
    fn initialize(&mut self) {
        let mut s = self.init.clone();
        s.sort_by(f32::total_cmp);
        if s[s.len() - 1] / s[0] >= 2.0 {
            // Split at the largest ratio gap between consecutive sorted values.
            let best_i = largest_ratio_gap(&s);
            self.lo = mean(&s[..=best_i]);
            self.hi = mean(&s[best_i + 1..]);
            self.confirmed = true;
            self.placeholder_is_lo = false;
        } else {
            let m = mean(&s);
            let assume_dah = self.unimodal_ceiling.is_some_and(|ceiling| m > ceiling);
            if assume_dah {
                self.hi = m;
                self.lo = m / 3.0;
                self.placeholder_is_lo = true;
            } else {
                self.lo = m;
                self.hi = 3.0 * m;
                self.placeholder_is_lo = false;
            }
            self.confirmed = false;
        }
        self.ready = true;
        self.init.clear();
    }

    fn reinit_from(&mut self, vals: &[f32]) {
        self.init = vals.to_vec();
        self.ready = false;
        self.confirmed = false;
        self.initialize();
    }
}

/// Mark-duration speed tracker. SPEC §4.1.
#[derive(Debug, Clone)]
pub struct SpeedTracker {
    pair: ClusterPair,
    ring: VecDeque<(f32, bool, f32, f32)>, // (dur_ms, assigned_dit, pre_lo, pre_hi)
    wpm_ema: Option<f32>,
    recent: VecDeque<f32>, // last 5 marks, reinit source
}

impl SpeedTracker {
    /// A tracker with no marks observed yet. SPEC §4.1.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        SpeedTracker {
            pair: ClusterPair::new(Some(DIT_CLAMP_MS.1)),
            ring: VecDeque::with_capacity(DRIFT_LEN),
            wpm_ema: None,
            recent: VecDeque::with_capacity(5),
        }
    }

    /// Whether enough marks have been observed to trust `mu_dit_ms`/`mu_dah_ms`. SPEC §4.1.
    pub fn ready(&self) -> bool {
        self.pair.ready()
    }

    /// Current dit-cluster centroid, in milliseconds. SPEC §4.1.
    pub fn mu_dit_ms(&self) -> f32 {
        self.pair.lo
    }

    /// Current dah-cluster centroid, in milliseconds. SPEC §4.1.
    pub fn mu_dah_ms(&self) -> f32 {
        self.pair.hi
    }

    /// The dit/dah decision boundary: geometric mean of the two centroids. SPEC §4.1.
    pub fn boundary_ms(&self) -> f32 {
        self.pair.boundary()
    }

    /// EMA-smoothed PARIS WPM (SPEC §4.1: 1200/mu_dit, alpha 0.1). None until ready.
    pub fn wpm(&self) -> Option<f32> {
        self.wpm_ema
    }

    /// Feed one mark duration, updating the clusters, constraints, and drift check. SPEC §4.1.
    pub fn on_mark(&mut self, dur_ms: f32) {
        self.recent.push_back(dur_ms);
        if self.recent.len() > 5 {
            self.recent.pop_front();
        }
        let pre_lo = self.pair.lo;
        let pre_hi = self.pair.hi;
        let was_init = self.pair.observe(dur_ms);
        if !self.pair.ready() {
            return;
        }
        self.apply_constraints();
        if !was_init {
            let is_dit = dur_ms < self.pair.boundary();
            self.ring.push_back((dur_ms, is_dit, pre_lo, pre_hi));
            if self.ring.len() > DRIFT_LEN {
                self.ring.pop_front();
            }
            self.check_drift();
        }
        let raw = 1200.0 / self.pair.lo;
        self.wpm_ema = Some(match self.wpm_ema {
            None => raw,
            Some(w) => w + WPM_ALPHA * (raw - w),
        });
    }

    fn apply_constraints(&mut self) {
        // SPEC §4.1: clamp mu_dit to [20, 150] ms; enforce 2.2 <= ratio <= 4.5.
        self.pair.lo = self.pair.lo.clamp(DIT_CLAMP_MS.0, DIT_CLAMP_MS.1);
        let ratio = self.pair.hi / self.pair.lo;
        if !(RATIO_MIN..=RATIO_MAX).contains(&ratio) {
            self.pair.hi = 3.0 * self.pair.lo;
        }
    }

    fn check_drift(&mut self) {
        // SPEC §4.1 regime change: 12 consecutive same-cluster marks, CV < 0.35,
        // mean off that centroid by > 40 % -> reinit from the last 5 marks.
        if self.ring.len() < DRIFT_LEN {
            return;
        }
        let all_dit = self.ring.iter().all(|&(_, d, _, _)| d);
        let all_dah = self.ring.iter().all(|&(_, d, _, _)| !d);
        if !all_dit && !all_dah {
            return;
        }
        let mut acc = 0.0f64;
        for &(d, _, _, _) in &self.ring {
            acc += d as f64;
        }
        let m = acc / self.ring.len() as f64;
        let mut var = 0.0f64;
        for &(d, _, _, _) in &self.ring {
            var += (d as f64 - m) * (d as f64 - m);
        }
        let cv = (var / self.ring.len() as f64).sqrt() / m;
        // Anchor to the centroid as it stood BEFORE this streak of DRIFT_LEN
        // marks began accumulating (ring[0]'s pre-mark snapshot) — comparing
        // against the LIVE centroid is self-defeating, since the same marks
        // driving the streak have already dragged it toward them by the time
        // the ring fills (pinned decision: SPEC §4.1's "off that centroid"
        // means the pre-streak centroid, not the continuously-adapting one).
        let (_, _, anchor_lo, anchor_hi) = self.ring[0];
        let centroid = if all_dit { anchor_lo } else { anchor_hi } as f64;
        if cv < DRIFT_CV_MAX && (m - centroid).abs() / centroid > DRIFT_OFF_FRAC {
            let vals: Vec<f32> = self.recent.iter().copied().collect();
            self.pair.reinit_from(&vals);
            self.apply_constraints();
            self.ring.clear();
            return;
        }
        // MAN-6: self-consistent bad lock. Twelve consecutive marks all
        // assigned to one cluster, yet those twelve durations themselves split
        // into two Morse-plausible clusters -- the boundary is on the wrong
        // side of a bimodal population and no amount of EMA will move it,
        // because the wrong centroid is what produced the assignments it is
        // being compared against. Reinitialize from the full 12-mark ring (not
        // `recent`'s 5) so the re-split has the most evidence available.
        if cv >= DRIFT_CV_MAX {
            let durs: Vec<f32> = self.ring.iter().map(|&(d, _, _, _)| d).collect();
            if is_credible_bimodal(&durs) {
                self.pair.reinit_from(&durs);
                self.apply_constraints();
                self.ring.clear();
            }
        }
    }
}

/// A classified inter-element gap. SPEC §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapClass {
    InterElement,
    InterChar,
    InterWord,
}

/// Gap classifier with Farnsworth decoupling. SPEC §4.2.
/// Long gaps are clustered in dit units (pinned decision 12).
#[derive(Debug, Clone)]
pub struct GapClassifier {
    pair: ClusterPair,
    long_seen: u32,
}

impl GapClassifier {
    /// A classifier with no gaps observed yet. SPEC §4.2.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        GapClassifier {
            pair: ClusterPair::new(None),
            long_seen: 0,
        }
    }

    fn farnsworth_active(&self) -> bool {
        self.pair.ready()
            && self.pair.confirmed()
            && self.long_seen >= FARNS_MIN_COUNT
            && self.pair.hi / self.pair.lo >= FARNS_MIN_RATIO
    }

    /// Scales decoder.rs's end-of-track safety-net flush multiple
    /// (`DecodeConfig::flush_gap_dits`, nominally applied as
    /// `nominal_flush_dits * mu_dit_ms`) to stay Farnsworth-aware. SPEC §4.2
    /// pins the flush trigger at `7*mu_dit`, sized to sit above the *fixed*
    /// `WORD_GAP_DITS = 5.0` nominal word-gap threshold -- comfortable
    /// headroom when `mu_dit` also represents the character/word spacing
    /// scale (the non-Farnsworth case). Under Farnsworth, `mu_dit` tracks
    /// only the (fast) content dit while real gaps are stretched by a much
    /// slower, decoupled spacing unit (SPEC §4.2's Farnsworth decoupling),
    /// so a flush multiple still anchored to the fixed `WORD_GAP_DITS`
    /// scale sits *below* real Farnsworth character gaps -- decoder.rs's
    /// safety net was firing on essentially every inter-character gap
    /// (confirmed via instrumented trace on the V10 vector:
    /// `flush_gap_dits * mu_dit_ms` computed to ~365-450ms once `mu_dit`
    /// stabilized, well under the real ~389-392ms character gap, while
    /// element gaps ~42ms and the occasional gap seen before Farnsworth
    /// activation were unaffected).
    ///
    /// Deliberately gated *more loosely* than `classify()`'s own
    /// `farnsworth_active()` (which additionally requires `long_seen >=
    /// FARNS_MIN_COUNT`): this method only needs `pair.ready() &&
    /// pair.confirmed() && hi/lo >= FARNS_MIN_RATIO`, dropping the sample-
    /// count gate. Reusing the stricter gate creates a measured chicken-
    /// and-egg failure -- while `long_seen < FARNS_MIN_COUNT`, this method
    /// would keep returning the unscaled nominal multiple, so
    /// `check_flush` keeps intercepting gaps *before* they ever reach
    /// `classify()`, which is the only place `long_seen` is incremented;
    /// `long_seen` can then never climb to `FARNS_MIN_COUNT` (confirmed via
    /// trace: with the stricter gate the V10 decode stayed in the
    /// "every-character-is-a-word" failure mode for several repeats of the
    /// text before happening to self-correct, instead of immediately after
    /// the pair's initial 5-sample bimodal split). This is directionally
    /// safe to loosen: this method only ever *raises* the flush threshold
    /// above `nominal_flush_dits` (see the `.max()` below), so an
    /// over-eager scale-up off a smaller, ratio-confirmed sample merely
    /// makes the safety net slower to fire, never incorrect -- unlike
    /// `classify()`'s own word/char decision, which must stay conservative
    /// since it's what the golden vectors' character stream depends on.
    pub fn flush_threshold_dits(&self, nominal_flush_dits: f32) -> f32 {
        let farnsworth_shaped = self.pair.ready()
            && self.pair.confirmed()
            && self.pair.hi / self.pair.lo >= FARNS_MIN_RATIO;
        if farnsworth_shaped {
            let ratio = self.pair.boundary() / WORD_GAP_DITS;
            (nominal_flush_dits * ratio).max(nominal_flush_dits)
        } else {
            nominal_flush_dits
        }
    }

    /// Classify one gap given the current dit estimate, incorporating it into the
    /// Farnsworth long-gap statistics if applicable. SPEC §4.2.
    pub fn classify(&mut self, gap_ms: f32, mu_dit_ms: f32) -> GapClass {
        let u = gap_ms / mu_dit_ms;
        // Thresholds from statistics BEFORE this gap is incorporated.
        let active = self.farnsworth_active();
        let word_thr = if active {
            self.pair.boundary() // sqrt(mu_cgap * mu_wgap), in dit units
        } else {
            WORD_GAP_DITS
        };
        let class = if u < CHAR_GAP_DITS {
            GapClass::InterElement
        } else if u < word_thr {
            GapClass::InterChar
        } else {
            GapClass::InterWord
        };
        if u >= FARNS_LONG_U {
            self.pair.observe(u);
            self.long_seen += 1;
        }
        class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 WPM nominal: dit 60 ms, dah 180 ms.
    fn feed(t: &mut SpeedTracker, durs: &[f32]) {
        for &d in durs {
            t.on_mark(d);
        }
    }

    #[test]
    fn initializes_bimodal_after_five_marks() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]); // C-ish opening
        assert!(t.ready());
        assert!(
            (t.mu_dit_ms() - 60.0).abs() < 1.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!((t.mu_dah_ms() - 180.0).abs() < 1.0);
        // B = sqrt(60*180) ~ 103.9
        assert!((t.boundary_ms() - 103.92).abs() < 0.5);
        assert!((t.wpm().unwrap() - 20.0).abs() < 0.5);
    }

    #[test]
    fn unimodal_init_provisional_then_reanchors() {
        // "EEE": all dits. SPEC §4.1: mu_dah = 3*mu_dit provisionally,
        // re-anchor when a mark lands >= 2*mu_dit.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 62.0, 58.0, 60.0, 61.0]);
        assert!(t.ready());
        assert!((t.mu_dah_ms() - 3.0 * t.mu_dit_ms()).abs() < 2.0);
        t.on_mark(185.0); // first real dah re-anchors
        assert!((t.mu_dah_ms() - 185.0).abs() < 0.1);
    }

    #[test]
    fn ratio_constraint_reanchors_dah() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        // Drag mu_dah down toward mu_dit with implausibly short dahs;
        // constraint 2.2..4.5 must re-anchor mu_dah = 3*mu_dit. SPEC §4.1.
        for _ in 0..40 {
            t.on_mark(115.0);
        }
        let ratio = t.mu_dah_ms() / t.mu_dit_ms();
        assert!((2.2..=4.5).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn dit_clamp_bounds_speed() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[10.0, 30.0, 10.0, 10.0, 30.0]); // 120 WPM: beyond clamp
        assert!(t.mu_dit_ms() >= 20.0); // SPEC §4.1 clamp [20, 150]
    }

    #[test]
    fn step_speed_change_reinitializes() {
        // QRQ: 20 WPM -> 35 WPM (dit 34 ms). Plain EMA can't follow a 43 % step;
        // the drift rule (12 consecutive single-cluster, CV < 0.35, off > 40 %)
        // must reinit. SPEC §4.1.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        for _ in 0..3 {
            feed(&mut t, &[60.0, 180.0, 60.0]);
        }
        for _ in 0..14 {
            t.on_mark(34.0); // fast dits, all far below the old dit centroid
        }
        assert!(
            (t.mu_dit_ms() - 34.0).abs() < 1.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
    }

    #[test]
    fn bimodal_badlock_recovers() {
        // MAN-6: force the exact self-consistent bad lock -- a fabricated 30 ms
        // first sample isolates itself as the whole dit cluster, so mu_dit = 30 and
        // the boundary lands below every real element. check_drift's existing rule
        // is blind to this (the 12-mark streak's CV is ~0.47, over DRIFT_CV_MAX),
        // so the new bimodal-lock branch must catch it.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[30.0, 84.0, 217.0, 84.0, 84.0]);
        assert!(
            t.boundary_ms() < 84.0,
            "precondition: bad lock not reproduced (boundary {})",
            t.boundary_ms()
        );
        // Now a long stream of genuine, correctly-measured elements.
        for _ in 0..6 {
            feed(&mut t, &[84.0, 217.0, 84.0, 84.0, 217.0]);
        }
        assert!(
            (t.mu_dit_ms() - 84.0).abs() < 12.0,
            "mu_dit failed to recover: {}",
            t.mu_dit_ms()
        );
        assert!(
            84.0 < t.boundary_ms() && t.boundary_ms() < 217.0,
            "boundary must separate dits from dahs: {}",
            t.boundary_ms()
        );
    }

    #[test]
    fn healthy_stream_never_triggers_badlock_recovery() {
        // A correctly-locked tracker sees marks in BOTH clusters, so the
        // all-one-cluster precondition never holds. 20 WPM, 60 marks.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        for _ in 0..11 {
            feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        }
        assert!(
            (t.mu_dit_ms() - 60.0).abs() < 3.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!(
            (t.mu_dah_ms() - 180.0).abs() < 8.0,
            "mu_dah {}",
            t.mu_dah_ms()
        );
    }

    #[test]
    fn jittery_single_cluster_does_not_trigger_badlock_recovery() {
        // A genuinely homogeneous but noisy run of dits must NOT be mistaken
        // for a bad lock. This data's CV is ~0.39 -- deliberately pushed past
        // DRIFT_CV_MAX (0.35) so check_drift's `cv >= DRIFT_CV_MAX` branch is
        // actually reached (a prior version of this data had CV 0.34 and
        // never reached that branch at all, passing vacuously) -- and its
        // largest-ratio-gap split gives a centroid ratio of ~1.98, below
        // RATIO_MIN (2.2), so the guard must reject it and leave the
        // (correct) 60 ms dit centroid alone.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        let jittered = [
            24.5, 32.6, 20.1, 26.6, 28.9, 29.4, 44.4, 43.9, 53.0, 41.4, 67.2, 70.6,
        ];
        feed(&mut t, &jittered);
        assert!(
            (t.mu_dit_ms() - 60.0).abs() < 20.0,
            "mu_dit should stay near 60, got {}",
            t.mu_dit_ms()
        );
    }

    #[test]
    fn gap_classification_nominal() {
        // CHAR_GAP_DITS boundary is 1.6, not SPEC §4.2's nominal 2.0 --
        // see docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md.
        let mut g = GapClassifier::new();
        let mu = 60.0;
        assert_eq!(g.classify(60.0, mu), GapClass::InterElement); // 1 dit
        assert_eq!(g.classify(180.0, mu), GapClass::InterChar); // 3 dits
        assert_eq!(g.classify(420.0, mu), GapClass::InterWord); // 7 dits
        assert_eq!(g.classify(95.0, mu), GapClass::InterElement); // < 1.6
        assert_eq!(g.classify(299.0, mu), GapClass::InterChar); // < 5.0
        assert_eq!(g.classify(300.0, mu), GapClass::InterWord); // >= 5.0
    }

    #[test]
    fn farnsworth_moves_word_threshold() {
        // Farnsworth: char gaps stretched to 6 dits, word gaps to 14 dits.
        // Nominal rule would call 6-dit gaps InterWord; after >= 8 long gaps
        // with ratio >= 1.8 the threshold becomes sqrt(6*14) ~ 9.2 dits. SPEC §4.2.
        let mut g = GapClassifier::new();
        let mu = 48.0;
        for _ in 0..5 {
            g.classify(6.0 * mu, mu);
            g.classify(14.0 * mu, mu);
        }
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(14.0 * mu, mu), GapClass::InterWord);
        // Element/char boundary is speed-locked, never Farnsworth-adjusted:
        assert_eq!(g.classify(1.5 * mu, mu), GapClass::InterElement);
    }

    #[test]
    fn unimodal_dah_init_assumes_dahs_not_dits() {
        // Pinned decision 20 fix: a lone ~180 ms cluster (all-dah opener, e.g.
        // "M", "O", "T T T") must be assumed dahs, not dits -- 180 ms exceeds
        // the SPEC §4.1 dit ceiling of 150 ms, so it cannot possibly be dits.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[180.0, 182.0, 178.0, 180.0, 181.0]);
        assert!(t.ready());
        assert!(
            (t.mu_dah_ms() - 180.2).abs() < 1.0,
            "mu_dah {}",
            t.mu_dah_ms()
        );
        assert!(
            (t.mu_dit_ms() - t.mu_dah_ms() / 3.0).abs() < 1.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
    }

    #[test]
    fn unimodal_dah_init_reanchors_on_first_real_dit() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[180.0, 182.0, 178.0, 180.0, 181.0]);
        t.on_mark(60.0); // first real dit re-anchors mu_dit immediately
        assert!(
            (t.mu_dit_ms() - 60.0).abs() < 0.1,
            "mu_dit {}",
            t.mu_dit_ms()
        );
    }

    #[test]
    fn unimodal_init_respects_ceiling_config() {
        // With a ceiling (SpeedTracker's use case): mean > ceiling assumes dahs.
        let mut with_ceiling = ClusterPair::new(Some(150.0));
        for &v in &[180.0, 182.0, 178.0, 180.0, 181.0] {
            with_ceiling.observe(v);
        }
        assert!(
            with_ceiling.placeholder_is_lo,
            "expected dah-assumed branch"
        );

        // Without a ceiling (GapClassifier's use case): a homogeneous cluster
        // whose mean would exceed 150 if it were milliseconds must NOT trip the
        // dah-assumed branch, since GapClassifier's values are dit-ratios, not
        // milliseconds -- pinned decision 20 follow-up.
        let mut no_ceiling = ClusterPair::new(None);
        for &v in &[200.0, 202.0, 198.0, 200.0, 201.0] {
            no_ceiling.observe(v);
        }
        assert!(
            !no_ceiling.placeholder_is_lo,
            "GapClassifier must always assume the low cluster is real"
        );
    }
}
