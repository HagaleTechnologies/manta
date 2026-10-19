//! Online 2-means speed tracking and gap classification. SPEC §4.1–§4.2.

use std::collections::VecDeque;

const CLUSTER_ALPHA: f32 = 0.15; // SPEC §9 decode.cluster_alpha
const RATIO_MIN: f32 = 2.2; // SPEC §9 decode.mu_ratio_bounds
                            // SPEC §9 decode.mu_ratio_bounds upper end, **[DEVIATION]** (MAN-315 D5):
                            // raised from 4.5 to 6.0. Heavy-dash senders (bug keys, weight 4-5) sit at
                            // or above 4.5, so the old ceiling re-anchored their dash centroid to
                            // 3 * mu_dit on every mark and a noise-seeded dit cluster never healed
                            // (measured: dit stuck at 20 ms for weight 4 and 5). See
                            // docs/DECISIONS/2026-10-11-man315-sender-timing.md.
const RATIO_MAX: f32 = 6.0;
const DIT_CLAMP_MS: (f32, f32) = (20.0, 150.0); // SPEC §4.1 (60..8 WPM)
const WPM_ALPHA: f32 = 0.1; // SPEC §4.1 reporting EMA
const DRIFT_LEN: usize = 12; // SPEC §4.1 regime-change rule
const DRIFT_CV_MAX: f64 = 0.35;
const DRIFT_OFF_FRAC: f64 = 0.40;
/// Two-sided cap on the mark/gap symmetry correction (`dit_estimate_ms`),
/// as a fraction of `mu_dit`. SPEC §4.1 **\[DEVIATION\]** (MAN-103 D6). Bounds
/// a runaway if mark/gap pairing ever breaks down (e.g. element gaps
/// swallowed by the 12 ms debounce at extreme WPM) while still admitting
/// the correction a correctly-placed SPEC §3.3 keying threshold requires --
/// see docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md.
const DIT_BIAS_CAP_FRAC: f32 = 0.35;
// SPEC §9 decode.char_gap_dits, SPEC §4.2 nominal value. Was lowered to 1.6
// (docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md) to compensate for
// Demod's old geometric-mean keying threshold (SPEC §3.2) systematically
// inflating every measured mark relative to true keyed timing, for every
// engine that fed off Demod. MAN-103 fixes that threshold at its source
// instead (additive band about the linear-amplitude midpoint, see
// docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md), so the
// downstream compensation this deviation existed for is gone -- retiring
// the `EdgeLegacy`/`Hsmm`-only `CHAR_GAP_DITS_NOMINAL` split (SPEC v2 §0
// Task 5) along with it, since every engine now uses the same nominal 2.0.
const CHAR_GAP_DITS: f32 = 2.0;
// SPEC §9 decode.word_gap_dits
const WORD_GAP_DITS: f32 = 5.0;
// SPEC §4.2 long-gap floor: the smallest gap, in dit units, that is
// admitted into the Farnsworth long-gap `ClusterPair`. Re-derived by
// MAN-103 together with `CHAR_GAP_DITS` (see
// docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md): the floor and
// the char/element boundary are the *same* boundary seen from two sides, so
// they must move together. It was 1.5 only because `CHAR_GAP_DITS` was
// temporarily 1.6; with the char boundary back at SPEC's nominal 2.0 a stale
// 1.5 opens a window [1.5, 2.0) of gaps that `classify` correctly calls
// InterElement while still folding them into the long-gap statistics. That
// window is not hypothetical at the top of the supported speed range: a
// plain element gap measures u ~ 1.5-1.6 near 40 WPM, which dragged
// `pair.lo` down to ~2 and `word_thr` with it until real inter-character
// gaps classified as InterWord (measured: "GQH AA" -> "GQH A A" at 39.95
// WPM, and CER 0.38 on a 40 WPM 60 s scene). Tying it to CHAR_GAP_DITS
// makes the two definitions of "long" identical by construction.
const FARNS_LONG_U: f32 = CHAR_GAP_DITS;
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
/// Bounds of the sender word boundary `SenderTiming::sender_word_dits`
/// hands `GapClassifier`, in dits. The floor keeps it above the 2.0
/// element/character boundary; gaps past the flush are flushed anyway.
/// SPEC §4.2 **\[DEVIATION\]** (MAN-315 D4).
const SENDER_WORD_DITS_CLAMP: (f32, f32) = (3.0, 12.0);
/// Observations both mark classes need before their mean ratio becomes
/// `SpeedTracker`'s re-anchor target (MAN-315 D5).
const RATIO_HINT_MIN_COUNT: u32 = 3;
/// Nominal word-gap/char-gap ratio (7/3): before any word gap is learned,
/// the sender word boundary sits at `char_gap * sqrt(7/3)`, the geometric
/// mean of the learned char gap and its nominal word gap (MAN-315 D4).
const NOMINAL_WORD_CHAR_RATIO: f32 = 7.0 / 3.0;
/// A closed word gap at or above `max(this, 4 * char-gap mean)` dits is a
/// pause, not the sender's word spacing, and is not learned (MAN-315 D2).
pub const PAUSE_MIN_DITS: f32 = 15.0;
/// MAN-271: the few marks a genuine end-of-signal finish may bootstrap the
/// 5-mark init quorum down to. SPEC §4.1 **\[DEVIATION\]** -- see
/// `SpeedTracker::bootstrap` and
/// docs/DECISIONS/2026-10-16-man271-short-transmission-decode.md.
const BOOTSTRAP_MIN_MARKS: usize = 2;

/// MAN-398: below this fraction of a streak's lower-quartile mark, the
/// element-gap centroid says the streak's marks really are dahs. A dah is
/// three element gaps long and a dit one, so the ratio sits near 1/3 for a
/// dah streak and near 1 for dits read as dahs; 1/sqrt(3) is the log-space
/// midpoint, the rule `ClusterPair::boundary` uses between mark clusters.
const BADLOCK_DAH_GAP_FRAC: f32 = 0.577_35;

fn mean(xs: &[f32]) -> f32 {
    let mut acc = 0.0f64;
    for &x in xs {
        acc += x as f64;
    }
    (acc / xs.len() as f64) as f32
}

/// Index of the last low-cluster value when ascending `s` is split at the
/// largest ratio between consecutive values (the first such, on a tie).
fn largest_ratio_split(s: &[f32]) -> usize {
    let mut best_i = 0;
    let mut best_r = 0.0f32;
    for i in 0..s.len() - 1 {
        let r = s[i + 1] / s[i];
        if r > best_r {
            best_r = r;
            best_i = i;
        }
    }
    best_i
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
    /// How many init samples the last `initialize()`'s bimodal split put in
    /// `lo` (0 after a unimodal init, whose `lo` is either the whole
    /// population or a placeholder). Read only by `SpeedTracker`'s MAN-398
    /// bad-lock check, which needs to know when `lo` rests on a single
    /// bootstrap sample.
    lo_n: usize,
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
            lo_n: 0,
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
            let best_i = largest_ratio_split(&s);
            self.lo = mean(&s[..=best_i]);
            self.hi = mean(&s[best_i + 1..]);
            self.confirmed = true;
            self.placeholder_is_lo = false;
            self.lo_n = best_i + 1;
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
            self.lo_n = 0;
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

    /// MAN-271: force the 5-observation init quorum down to `min` for a
    /// genuine end-of-signal flush (`SpeedTracker::bootstrap`). Returns
    /// false (and changes nothing) when already ready or under `min`
    /// observations; otherwise the ordinary `initialize` split runs over
    /// what arrived. The split rule itself is unchanged: it works for any
    /// count >= 1, only `observe`'s fixed 5 gates it.
    fn force_initialize(&mut self, min: usize) -> bool {
        if self.ready || self.init.len() < min {
            return false;
        }
        self.initialize();
        true
    }
}

/// Mark-duration speed tracker. SPEC §4.1.
#[derive(Debug, Clone)]
pub struct SpeedTracker {
    pair: ClusterPair,
    ring: VecDeque<(f32, bool, f32, f32)>, // (dur_ms, assigned_dit, pre_lo, pre_hi)
    wpm_ema: Option<f32>,
    recent: VecDeque<f32>, // last 5 marks, reinit source
    /// Inter-element gap centroid, in ms. SPEC §4.1 **\[DEVIATION\]** (MAN-103
    /// D6): feeds `dit_estimate_ms`'s mark/gap symmetry correction.
    mu_egap_ms: Option<f32>,
    /// The sender's learned dash/dot ratio, the re-anchor target
    /// `apply_constraints` uses in place of the ideal 3.0. SPEC §4.1
    /// **\[DEVIATION\]** (MAN-315 D5).
    ratio_hint: Option<f32>,
    /// Generation counter, bumped whenever a drift re-init rebuilds the
    /// mark scale, so a caller holding statistics in the old scale's units
    /// knows to drop them (MAN-315 D7).
    regime: u32,
    /// MAN-398: set for exactly one `on_mark()` call after the bad-lock
    /// branch of `check_drift` reinitializes `pair`, then cleared by
    /// `take_badlock_recovered()`. `GapClassifier` has its own `ClusterPair`
    /// bootstrapped from `gap_ms / mu_dit_ms` ratios computed against
    /// whatever `mu_dit_ms` was in effect at the time -- during a bad lock
    /// that is the corrupted value, so its cluster boundary latches onto a
    /// wrong scale and never self-corrects even after this tracker recovers
    /// (`ClusterPair` has no drift/reinit machinery of its own; only
    /// `SpeedTracker::check_drift` calls `reinit_from`). Without a signal to
    /// reset it too, every inter-word gap keeps misclassifying as
    /// inter-char forever, silently deleting every word boundary from the
    /// recovery point on. See
    /// docs/DECISIONS/2026-10-18-man398-gap-corroborated-badlock-recovery.md.
    badlock_recovered: bool,
    /// MAN-398: true while `mu_dit` still rests on the
    /// single bootstrap sample `ClusterPair::initialize`'s split isolated as
    /// the whole dit cluster, with no later mark assigned to it. Cleared by
    /// the first post-bootstrap dit and by any reinit.
    lo_uncorroborated: bool,
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
            mu_egap_ms: None,
            ratio_hint: None,
            regime: 0,
            badlock_recovered: false,
            lo_uncorroborated: false,
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

    /// EMA-smoothed PARIS WPM (SPEC §4.1a: 1200/dit_estimate_ms, alpha 0.1). None until ready.
    pub fn wpm(&self) -> Option<f32> {
        self.wpm_ema
    }

    /// Set (or clear) the dash/dot ratio `apply_constraints` re-anchors
    /// `mu_dah` to when the ratio leaves `[2.2, 6.0]`. `None` restores the
    /// ideal 3.0. The hint is clamped to the same bounds. SPEC §4.1
    /// **\[DEVIATION\]** (MAN-315 D5).
    pub fn set_ratio_hint(&mut self, hint: Option<f32>) {
        self.ratio_hint = hint
            .filter(|r| r.is_finite())
            .map(|r| r.clamp(RATIO_MIN, RATIO_MAX));
    }

    /// The mark-scale generation, bumped by a drift re-init (MAN-315 D7).
    pub fn regime(&self) -> u32 {
        self.regime
    }

    /// Current inter-element gap centroid, in milliseconds, if any
    /// inter-element gaps have been observed yet. SPEC §4.1 **\[DEVIATION\]**.
    pub fn mu_egap_ms(&self) -> Option<f32> {
        self.mu_egap_ms
    }

    /// Feed one inter-element gap duration (SPEC §4.2's `InterElement`
    /// class), updating the gap centroid `dit_estimate_ms` uses. SPEC §4.1
    /// **\[DEVIATION\]** (MAN-103 D6/D8).
    pub fn on_element_gap(&mut self, dur_ms: f32) {
        if !dur_ms.is_finite() || dur_ms <= 0.0 {
            return;
        }
        self.mu_egap_ms = Some(match self.mu_egap_ms {
            None => dur_ms,
            Some(g) => g + CLUSTER_ALPHA * (dur_ms - g),
        });
    }

    /// The dit period used for PARIS WPM reporting: `mu_dit` corrected by
    /// half the mark/gap asymmetry against `mu_egap`, two-sided-capped at
    /// `DIT_BIAS_CAP_FRAC * mu_dit`. SPEC §4.1 **\[DEVIATION\]** (MAN-103 D6,
    /// adopting MAN-7's symmetric-period design with a two-sided cap -- see
    /// docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md). A
    /// keying-edge crossing only moves the mark/space *boundary*; it
    /// neither creates nor destroys time, so `mu_dit + mu_egap == 2 *
    /// true_dit` regardless of where that boundary sits -- including the
    /// residual left by a correctly-placed SPEC §3.3 threshold, which is
    /// exactly the transmitter's own raised-cosine rise time (shortening
    /// every mark between its 50 % points and lengthening every gap by the
    /// same amount). `mu_dit_ms()` itself stays uncorrected: its other
    /// consumers (§4.3's beam likelihoods, §3.2's `tau_hi`) want a centroid
    /// consistent with the marks actually being classified. Gaps are the
    /// other side of that boundary, so since MAN-315 (D3) SPEC §4.2's `u`
    /// measures them in this symmetric period too: in mark dits the rise
    /// time made every gap read long, and a 4.5-dit letter gap crossed the
    /// 5.0 word boundary.
    pub fn dit_estimate_ms(&self) -> f32 {
        let mu_dit = self.pair.lo;
        let Some(g) = self.mu_egap_ms else {
            return mu_dit;
        };
        let cap = DIT_BIAS_CAP_FRAC * mu_dit;
        let delta = (0.5 * (mu_dit - g)).clamp(-cap, cap);
        (mu_dit - delta).clamp(DIT_CLAMP_MS.0, DIT_CLAMP_MS.1)
    }

<<<<<<< HEAD
    /// True exactly once after the MAN-398 bad-lock branch has fired
    /// since the last call; cleared on read. See `badlock_recovered`'s doc
    /// comment for why callers (`TrackDecoder`) need this to also reset
    /// `GapClassifier`.
    pub fn take_badlock_recovered(&mut self) -> bool {
        std::mem::take(&mut self.badlock_recovered)
=======
    /// MAN-271 (SPEC §4.1 **\[DEVIATION\]**): initialize the mark clusters
    /// from the `BOOTSTRAP_MIN_MARKS`..4 marks a short single transmission
    /// leaves, at a genuine end-of-signal flush (`TrackDecoder::finish` --
    /// stream EOF or a HangExpired/Silent closure, where a sustained real
    /// on-air gap has been observed). Two marks is the least a dit/dah
    /// ratio can be split from; the ordinary `initialize` rule
    /// (largest-ratio split, unimodal fallback) runs over them unchanged.
    /// Returns false (and changes nothing) when the tracker is already
    /// ready or saw fewer than two marks -- `Hsmm`, which never feeds the
    /// tracker, is a no-op. Only `finish` may call this:
    /// `finish_speed_only` (bookkeeping closes: Merged/Evicted) must never
    /// fabricate content, and bootstrapping mid-stream would need a
    /// dit-unit threshold before any dit estimate exists.
    pub fn bootstrap(&mut self) -> bool {
        if !self.pair.force_initialize(BOOTSTRAP_MIN_MARKS) {
            return false;
        }
        self.apply_constraints();
        self.wpm_ema = Some(1200.0 / self.dit_estimate_ms());
        true
>>>>>>> a1761da5598d5f338c00da0dc6faa3578d5b7bf2
    }

    /// Feed one mark duration, updating the clusters, constraints, and drift check. SPEC §4.1.
    pub fn on_mark(&mut self, dur_ms: f32) {
        self.recent.push_back(dur_ms);
        if self.recent.len() > 5 {
            self.recent.pop_front();
        }
        let pre_lo = self.pair.lo;
        let pre_hi = self.pair.hi;
        let was_ready = self.pair.ready();
        let was_init = self.pair.observe(dur_ms);
        if !self.pair.ready() {
            return;
        }
        if !was_ready {
            self.lo_uncorroborated = self.pair.lo_n == 1;
        }
        self.apply_constraints();
        if !was_init {
            let is_dit = dur_ms < self.pair.boundary();
            if is_dit {
                self.lo_uncorroborated = false;
            }
            self.ring.push_back((dur_ms, is_dit, pre_lo, pre_hi));
            if self.ring.len() > DRIFT_LEN {
                self.ring.pop_front();
            }
            self.check_drift();
        }
        let raw = 1200.0 / self.dit_estimate_ms();
        self.wpm_ema = Some(match self.wpm_ema {
            None => raw,
            Some(w) => w + WPM_ALPHA * (raw - w),
        });
    }

    /// Replace the WPM seed after the decoder's warmup `pending` replay
    /// (MAN-265). The mark that made the tracker ready seeded `wpm_ema`
    /// inside `on_mark` before any element gap existed, so a transmission
    /// ending at that boundary kept the mark-only estimate forever.
    /// Assigns rather than EMA-folds: this is still the first sample (the
    /// replayed marks were already counted, and its gaps only reached
    /// `mu_egap` during the replay), so there is no smoothing history to
    /// disturb -- later live marks keep the normal `WPM_ALPHA` steps.
    /// No-op unless ready. SPEC §4.1a **\[DEVIATION\]** (MAN-265).
    pub(crate) fn reseed_wpm_after_warmup(&mut self) {
        if self.ready() {
            self.wpm_ema = Some(1200.0 / self.dit_estimate_ms());
        }
    }

    fn apply_constraints(&mut self) {
        // SPEC §4.1: clamp mu_dit to [20, 150] ms; enforce 2.2 <= ratio <= 6.0
        // (MAN-315 D5: was 4.5), re-anchoring to the sender's learned ratio
        // when one is known, else the ideal 3.0.
        self.pair.lo = self.pair.lo.clamp(DIT_CLAMP_MS.0, DIT_CLAMP_MS.1);
        let ratio = self.pair.hi / self.pair.lo;
        if !(RATIO_MIN..=RATIO_MAX).contains(&ratio) {
            self.pair.hi = self.ratio_hint.unwrap_or(3.0) * self.pair.lo;
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
            // A stale gap centroid from the old speed regime must not carry
            // into the new one's dit_estimate_ms correction (MAN-103 D6).
            self.mu_egap_ms = None;
            self.regime = self.regime.wrapping_add(1);
            self.lo_uncorroborated = false;
            return;
        }
        // MAN-398: a fragment-seeded lock. A track attached mid-element hands the
        // 5-mark bootstrap a short first run with no real leading edge; the split
        // isolates it as the whole dit cluster and every real mark lands in the dah
        // cluster. The SPEC rule above cannot see it (the streak sits on its own
        // centroid). Re-seed from the streak when the dit cluster still rests on
        // that lone bootstrap mark and the gap scale does not say the streak's marks
        // are dahs. No CV term: dit-heavy text with some dahs (5NN, HV) streaks with
        // CV ~0.6. See docs/DECISIONS/2026-10-18-man398-gap-corroborated-badlock-recovery.md.
        if all_dah && self.lo_uncorroborated && !self.streak_marks_are_dahs() {
            let durs: Vec<f32> = self.ring.iter().map(|&(d, _, _, _)| d).collect();
            self.pair.reinit_from(&durs);
            self.apply_constraints();
            self.ring.clear();
            self.lo_uncorroborated = false;
            self.mu_egap_ms = None;
            self.regime = self.regime.wrapping_add(1);
            self.badlock_recovered = true;
        }
    }

    /// MAN-398: whether the gap scale says the 12-mark streak's marks are dahs:
    /// the element-gap centroid is under `BADLOCK_DAH_GAP_FRAC` of the streak's
    /// lower-quartile mark. `None` is not evidence of dahs: against a fragment's
    /// dit unit every real element gap reads as a character gap, so the
    /// centroid starves exactly when the lock is wrong. Compared with the
    /// streak, not `mu_dit`: in a fragment lock on all-dah text `mu_dit` is the
    /// fragment and disagrees with every gap, although the dahs are right.
    fn streak_marks_are_dahs(&self) -> bool {
        let Some(g) = self.mu_egap_ms else {
            return false;
        };
        let mut d: Vec<f32> = self.ring.iter().map(|&(d, _, _, _)| d).collect();
        d.sort_by(f32::total_cmp);
        g < BADLOCK_DAH_GAP_FRAC * d[d.len() / 4]
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
    char_gap_dits: f32,
    /// Set when `reinit_from_flushed` rebuilt `pair`; cleared when an
    /// ordinary character gap disproves the rebuild (see `classify`), or
    /// when `FARNS_MIN_COUNT` word gaps confirm it (`confirm_rebuilt`).
    rebuilt: bool,
    /// Word gaps `confirm_rebuilt` has counted since the rebuild.
    rebuilt_confirms: u32,
    /// Classified gaps from `char_gap_dits` up to this many dits disprove
    /// a rebuild: `WORD_GAP_DITS`, or less when the flush is configured
    /// below it (`reinit_from_flushed`).
    rebuilt_disproof_dits: f32,
    /// The sender's own character/word boundary, in dits, used by
    /// `classify` in place of `WORD_GAP_DITS` while Farnsworth is not
    /// active. SPEC §4.2 **\[DEVIATION\]** (MAN-315 D4).
    sender_word_dits: Option<f32>,
}

impl GapClassifier {
    /// A classifier with no gaps observed yet, using the SPEC §4.2 nominal
    /// `CHAR_GAP_DITS` boundary (2.0; see the constant's doc comment) --
    /// shared by all three decode engines since MAN-103.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::new_with(CHAR_GAP_DITS)
    }

    /// A classifier with an explicit inter-element/inter-char boundary, in
    /// dit units, for a test or caller that needs a non-default value;
    /// `new()` covers every production decode engine.
    pub fn new_with(char_gap_dits: f32) -> Self {
        GapClassifier {
            pair: ClusterPair::new(None),
            long_seen: 0,
            char_gap_dits,
            rebuilt: false,
            rebuilt_confirms: 0,
            rebuilt_disproof_dits: WORD_GAP_DITS,
            sender_word_dits: None,
        }
    }

    /// Set (or clear) the sender word boundary, in dits, that `classify`
    /// uses while Farnsworth is not active; `None` restores the nominal
    /// `WORD_GAP_DITS`. The Farnsworth pair still wins when active. SPEC
    /// §4.2 **\[DEVIATION\]** (MAN-315 D4).
    pub fn set_sender_word_dits(&mut self, dits: Option<f32>) {
        self.sender_word_dits = dits.filter(|d| d.is_finite());
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

    /// Fold a gap that the decoder resolved OUTSIDE `classify` -- SPEC
    /// §4.2's 7-dit safety-net flush -- into the Farnsworth long-gap
    /// statistics.
    ///
    /// `classify` is the only place `long_seen` and the long-gap
    /// `ClusterPair` normally advance, and `check_flush` bypasses it. That
    /// was harmless while `mu_dit` ran high (pre-MAN-103): the flush
    /// threshold in ms sat above real Farnsworth character gaps. With
    /// `mu_dit` corrected (MAN-103), `flush_gap_dits * mu_dit` drops below
    /// them, the safety net intercepts nearly every character gap, and the
    /// Farnsworth bootstrap can never complete -- V10 decoded every
    /// character as its own word (MAN-103 D8, measured CER 0.2368).
    pub fn observe_flushed(&mut self, gap_ms: f32, mu_dit_ms: f32) {
        let u = gap_ms / mu_dit_ms;
        if u >= FARNS_LONG_U {
            self.pair.observe(u);
            self.long_seen += 1;
        }
    }

    /// Rebuild the long-gap statistics from `us`, the closed lengths (dit
    /// units) of the latest gaps that `check_flush` force-flushed after a
    /// one-character word, with `nominal_flush_dits` the configured flush
    /// multiple (`DecodeConfig::flush_gap_dits`). Returns whether it rebuilt
    /// them.
    ///
    /// `observe_flushed` censors every flushed gap at the flush threshold
    /// (~7 dits), which keeps a lone pause out of the statistics but also
    /// hides heavy Farnsworth spacing: when even the character gap outruns
    /// the threshold (18/5, 20/8, 25/10 WPM), character and word gaps both
    /// enter as ~7, the pair never splits, and every character decodes as
    /// its own word for the whole transmission. A run of one-character
    /// flushed words is that failure's signature, and the closed lengths
    /// carry the real spacing. Only a clean split rebuilds: at least two
    /// gaps in each cluster, and a ratio of at least `FARNS_MIN_RATIO`
    /// between the clusters' nearest members. So one pause among them
    /// cannot set the word-gap cluster (`decoder.rs`'s
    /// `legacy_early_pause_does_not_merge_later_words`), and neither can
    /// irregular fragment spacing with no gap between clusters. Two pauses
    /// can, at ordinary spacing, with the word gaps as the low cluster
    /// (MAN-264). `classify` discards such a rebuild at the first ordinary
    /// character gap, unless `confirm_rebuilt` has counted
    /// `FARNS_MIN_COUNT` word gaps first. With `nominal_flush_dits` below
    /// `WORD_GAP_DITS`, genuine character gaps between the two outran the
    /// flush and built the pair, so they disprove nothing. The bound is
    /// then the larger of `nominal_flush_dits`, below which a gap cannot
    /// have outrun the flush, and the rebuilt low cluster over
    /// `FARNS_MIN_RATIO`, below which a gap is no member of it.
    pub fn reinit_from_flushed(&mut self, us: &[f32], nominal_flush_dits: f32) -> bool {
        let mut s = us.to_vec();
        s.sort_by(f32::total_cmp);
        let n = s.len();
        if n < 4 || s[n - 1] / s[0] < 2.0 {
            return false;
        }
        let best_i = largest_ratio_split(&s);
        if best_i < 1 || n - best_i - 1 < 2 || s[best_i + 1] / s[best_i] < FARNS_MIN_RATIO {
            return false;
        }
        self.pair.reinit_from(us);
        self.long_seen = self.long_seen.max(n as u32);
        self.rebuilt = true;
        self.rebuilt_confirms = 0;
        self.rebuilt_disproof_dits = nominal_flush_dits
            .max(self.pair.lo / FARNS_MIN_RATIO)
            .min(WORD_GAP_DITS);
        true
    }

    /// Count a gap that ended a decoded word of two or more characters,
    /// classified or flushed, toward confirming a rebuilt pair. Once
    /// `FARNS_MIN_COUNT` have arrived, the rebuild stands, so a fade that
    /// drops a dit inside a Farnsworth character cannot undo it. The
    /// decoder's word count is the guard: it passes no gap after a decoded
    /// one-character word, since those gaps built the rebuild. After a
    /// false rebuild, ordinary word gaps classify as `InterChar`, so they
    /// end no word and never arrive here (MAN-264). The gap must reach the
    /// boundary. `flushed` is `Some(nominal_flush_dits)` for a gap
    /// `check_flush` cut off at the flush threshold, `None` for a closed
    /// gap. A flushed gap's bar is the boundary scaled by
    /// `nominal_flush_dits / WORD_GAP_DITS` when the flush is configured
    /// below `WORD_GAP_DITS`: while the pair is Farnsworth-shaped,
    /// `flush_threshold_dits` cuts word gaps off that far below the
    /// boundary. The check only matters once the pair has drifted out of
    /// Farnsworth shape: before that, every gap the decoder passes reaches
    /// its bar.
    pub fn confirm_rebuilt(&mut self, gap_ms: f32, mu_dit_ms: f32, flushed: Option<f32>) {
        let b = self.pair.boundary();
        let bar = flushed.map_or(b, |nominal_flush_dits| {
            (nominal_flush_dits * (b / WORD_GAP_DITS)).min(b)
        });
        if self.rebuilt && gap_ms / mu_dit_ms >= bar {
            self.rebuilt_confirms += 1;
            self.rebuilt = self.rebuilt_confirms < FARNS_MIN_COUNT;
        }
    }

    /// Classify one gap given the current dit estimate, incorporating it into the
    /// Farnsworth long-gap statistics if applicable. SPEC §4.2.
    pub fn classify(&mut self, gap_ms: f32, mu_dit_ms: f32) -> GapClass {
        let u = gap_ms / mu_dit_ms;
        // A rebuild from flushed one-character words assumes every
        // character gap outran the flush. A character gap below
        // `rebuilt_disproof_dits` disproves it: the window held word gaps
        // and pauses, and the rebuilt boundary sits above every real word
        // gap. Start the long-gap statistics over.
        if self.rebuilt && u >= self.char_gap_dits && u < self.rebuilt_disproof_dits {
            self.pair = ClusterPair::new(None);
            self.long_seen = 0;
            self.rebuilt = false;
        }
        // Thresholds from statistics BEFORE this gap is incorporated.
        let active = self.farnsworth_active();
        let word_thr = if active {
            self.pair.boundary() // sqrt(mu_cgap * mu_wgap), in dit units
        } else {
            // MAN-315 D4: the sender's own boundary when one is learned.
            self.sender_word_dits.unwrap_or(WORD_GAP_DITS)
        };
        let class = if u < self.char_gap_dits {
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

/// One of the five per-sender duration classes `SenderTiming` learns.
/// SPEC §4.1b (MAN-315).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingClass {
    Dot,
    Dash,
    ElementGap,
    CharGap,
    WordGap,
}

impl TimingClass {
    fn index(self) -> usize {
        self as usize
    }
}

/// Exponentially weighted mean and variance of one class, in dits.
#[derive(Debug, Clone, Copy, Default)]
struct ClassStats {
    mean: f32,
    var: f32,
    n: u32,
}

impl ClassStats {
    /// SPEC §4.1b: `d = x - mean; mean += a*d; var = (1-a)*(var + a*d^2)`,
    /// `a = CLUSTER_ALPHA`; the first sample sets `mean = x`, `var = 0`.
    fn observe(&mut self, x: f32) {
        if self.n == 0 {
            self.mean = x;
            self.var = 0.0;
        } else {
            let d = x - self.mean;
            self.mean += CLUSTER_ALPHA * d;
            self.var = (1.0 - CLUSTER_ALPHA) * (self.var + CLUSTER_ALPHA * d * d);
        }
        self.n = self.n.saturating_add(1);
    }

    fn estimate(&self) -> Option<ClassEstimate> {
        (self.n > 0).then_some(ClassEstimate {
            mean_dits: self.mean,
            var_dits2: self.var,
            n: self.n,
        })
    }
}

/// One class's learned duration: mean and variance in dits, and how many
/// observations they rest on. SPEC §4.1b / §5 `TimingUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassEstimate {
    pub mean_dits: f32,
    pub var_dits2: f32,
    pub n: u32,
}

/// A track's five learned duration classes, in dits of `dit_ms` (the
/// symmetric dit period `SpeedTracker::dit_estimate_ms`, the one WPM is
/// reported from). `None` for a class with no observations yet. SPEC §5
/// `TimingUpdate` (MAN-315 D8).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TimingEstimates {
    pub dit_ms: f32,
    pub dot: Option<ClassEstimate>,
    pub dash: Option<ClassEstimate>,
    pub element_gap: Option<ClassEstimate>,
    pub char_gap: Option<ClassEstimate>,
    pub word_gap: Option<ClassEstimate>,
}

impl TimingEstimates {
    /// One-line human rendering, `dot 0.91 ± 0.05  dash 2.93 ± 0.12 ...`
    /// (mean ± standard deviation, dits; `-` for a class with no
    /// observations). Used by `manta decode`'s timing line.
    pub fn display_line(&self) -> String {
        let fmt = |c: &Option<ClassEstimate>| match c {
            Some(c) => format!("{:.2} ± {:.2}", c.mean_dits, c.var_dits2.max(0.0).sqrt()),
            None => "-".to_string(),
        };
        format!(
            "dot {}  dash {}  element gap {}  char gap {}  word gap {}",
            fmt(&self.dot),
            fmt(&self.dash),
            fmt(&self.element_gap),
            fmt(&self.char_gap),
            fmt(&self.word_gap)
        )
    }
}

/// Per-sender timing model (MAN-315): dot, dash, element-gap, char-gap and
/// word-gap durations, each an exponentially weighted mean and variance in
/// dits with an observation count. One per `TrackDecoder`, so each track
/// learns its own sender. SPEC §4.1b.
///
/// Values are stored in dits of the dit period at observation time (the
/// caller passes `SpeedTracker::dit_estimate_ms`), so ratios survive slow
/// speed drift. MAN-111 (scored boundaries) and MAN-113 (WPM hypotheses)
/// read `estimates()`; this model itself uses only the means.
#[derive(Debug, Clone, Default)]
pub struct SenderTiming {
    stats: [ClassStats; 5],
}

impl SenderTiming {
    /// A model with no observations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one duration of `class`, `dur_ms` long, measured against a dit
    /// of `dit_ms`. Non-finite or non-positive values are ignored.
    pub fn observe(&mut self, class: TimingClass, dur_ms: f32, dit_ms: f32) {
        let x = dur_ms / dit_ms;
        if !(dur_ms > 0.0 && dit_ms > 0.0 && x.is_finite()) {
            return;
        }
        self.stats[class.index()].observe(x);
    }

    /// The learned mean of `class`, in dits, if it has any observations.
    pub fn mean_dits(&self, class: TimingClass) -> Option<f32> {
        let s = &self.stats[class.index()];
        (s.n > 0).then_some(s.mean)
    }

    /// Observations of `class` so far.
    pub fn count(&self, class: TimingClass) -> u32 {
        self.stats[class.index()].n
    }

    /// Observations across all classes (the `TimingUpdate` change test).
    pub fn total_count(&self) -> u64 {
        self.stats.iter().map(|s| u64::from(s.n)).sum()
    }

    /// The sender's dash/dot ratio, once both mark classes have at least
    /// `RATIO_HINT_MIN_COUNT` observations (MAN-315 D5).
    pub fn dash_dot_ratio(&self) -> Option<f32> {
        let dot = &self.stats[TimingClass::Dot.index()];
        let dash = &self.stats[TimingClass::Dash.index()];
        (dot.n >= RATIO_HINT_MIN_COUNT && dash.n >= RATIO_HINT_MIN_COUNT && dot.mean > 0.0)
            .then(|| dash.mean / dot.mean)
            .filter(|r| r.is_finite())
    }

    /// The sender's character/word boundary, in dits (MAN-315 D4): the
    /// geometric mean of the learned char-gap and word-gap means; with no
    /// word gap learned yet, the char gap times `sqrt(7/3)`; `None` before
    /// any char gap. Clamped to `SENDER_WORD_DITS_CLAMP`.
    pub fn sender_word_dits(&self) -> Option<f32> {
        let cg = self.mean_dits(TimingClass::CharGap)?;
        let b = match self.mean_dits(TimingClass::WordGap) {
            Some(wg) => (cg * wg).sqrt(),
            None => cg * NOMINAL_WORD_CHAR_RATIO.sqrt(),
        };
        b.is_finite()
            .then(|| b.clamp(SENDER_WORD_DITS_CLAMP.0, SENDER_WORD_DITS_CLAMP.1))
    }

    /// Whether a closed word gap of `dits` is a pause rather than the
    /// sender's word spacing: at least `max(PAUSE_MIN_DITS, 4 * char-gap
    /// mean)` (MAN-315 D2).
    pub fn is_pause(&self, dits: f32) -> bool {
        let cg = self.mean_dits(TimingClass::CharGap).unwrap_or(0.0);
        dits >= PAUSE_MIN_DITS.max(4.0 * cg)
    }

    /// Forget every class (a new speed regime, MAN-315 D7).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The five classes as a report, with `dit_ms` the reference dit.
    pub fn estimates(&self, dit_ms: f32) -> TimingEstimates {
        let e = |c: TimingClass| self.stats[c.index()].estimate();
        TimingEstimates {
            dit_ms,
            dot: e(TimingClass::Dot),
            dash: e(TimingClass::Dash),
            element_gap: e(TimingClass::ElementGap),
            char_gap: e(TimingClass::CharGap),
            word_gap: e(TimingClass::WordGap),
        }
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

    /// MAN-271: "DA" @ 35 WPM reaches the decoder as these four marks
    /// (promotion consumed the leading element); the live quorum never
    /// fills, a genuine end-of-signal finish bootstraps from them.
    #[test]
    fn bootstrap_initializes_from_four_marks() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[32.0, 34.7, 32.0, 104.0]);
        assert!(!t.ready());
        assert!(t.bootstrap());
        assert!(t.ready());
        assert!(
            (t.mu_dit_ms() - 32.9).abs() < 0.5,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!(
            (t.mu_dah_ms() - 104.0).abs() < 0.5,
            "mu_dah {}",
            t.mu_dah_ms()
        );
        assert!(t.wpm().is_some());
    }

    /// MAN-271: one mark carries no dit/dah ratio to split; zero marks
    /// even less. The bootstrap refuses and the tracker stays not ready.
    #[test]
    fn bootstrap_needs_two_marks() {
        let mut t = SpeedTracker::new();
        assert!(!t.bootstrap());
        assert!(!t.ready());
        let mut t = SpeedTracker::new();
        feed(&mut t, &[32.0]);
        assert!(!t.bootstrap());
        assert!(!t.ready());
    }

    /// MAN-271: once the ordinary quorum filled, the bootstrap is a no-op.
    #[test]
    fn bootstrap_is_a_no_op_once_ready() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        assert!(t.ready());
        let (dit, dah) = (t.mu_dit_ms(), t.mu_dah_ms());
        assert!(!t.bootstrap());
        assert!((t.mu_dit_ms() - dit).abs() < 1e-6);
        assert!((t.mu_dah_ms() - dah).abs() < 1e-6);
    }

    #[test]
    fn ratio_constraint_reanchors_dah() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        // Drag mu_dah down toward mu_dit with implausibly short dahs;
        // constraint 2.2..6.0 must re-anchor mu_dah = 3*mu_dit. SPEC §4.1.
        // MAN-315 D5: the upper bound was 4.5.
        for _ in 0..40 {
            t.on_mark(115.0);
        }
        let ratio = t.mu_dah_ms() / t.mu_dit_ms();
        assert!((2.2..=6.0).contains(&ratio), "ratio {ratio}");
    }

    /// MAN-315 D1: per-class EW mean and variance, in dits.
    #[test]
    fn sender_timing_learns_heavy_dash_mean_and_variance() {
        let mut st = SenderTiming::new();
        for i in 0..40 {
            st.observe(TimingClass::Dot, 48.0, 48.0);
            let jitter = if i % 2 == 0 { 4.0 } else { -4.0 };
            st.observe(TimingClass::Dash, 192.0 + jitter, 48.0);
        }
        let est = st.estimates(48.0);
        let dash = est.dash.unwrap();
        let dot = est.dot.unwrap();
        assert!((dash.mean_dits - 4.0).abs() < 0.05, "dash {dash:?}");
        assert!((dot.mean_dits - 1.0).abs() < 1e-6, "dot {dot:?}");
        assert!(dash.var_dits2 > 0.0, "dash {dash:?}");
        assert_eq!(dot.var_dits2, 0.0);
        assert_eq!((dot.n, dash.n), (40, 40));
        assert!((st.dash_dot_ratio().unwrap() - 4.0).abs() < 0.05);
        assert_eq!(st.total_count(), 80);
        assert!(est.element_gap.is_none() && est.char_gap.is_none() && est.word_gap.is_none());
        // Non-finite and non-positive durations are ignored.
        st.observe(TimingClass::Dot, 0.0, 48.0);
        st.observe(TimingClass::Dot, 48.0, 0.0);
        st.observe(TimingClass::Dot, f32::NAN, 48.0);
        assert_eq!(st.count(TimingClass::Dot), 40);
    }

    #[test]
    fn sender_timing_classes_are_independent() {
        let mut st = SenderTiming::new();
        for _ in 0..10 {
            st.observe(TimingClass::Dot, 50.0, 50.0);
            st.observe(TimingClass::Dash, 150.0, 50.0);
        }
        let marks = (
            st.mean_dits(TimingClass::Dot),
            st.mean_dits(TimingClass::Dash),
        );
        for _ in 0..10 {
            st.observe(TimingClass::ElementGap, 60.0, 50.0);
            st.observe(TimingClass::CharGap, 225.0, 50.0);
            st.observe(TimingClass::WordGap, 450.0, 50.0);
        }
        assert_eq!(
            (
                st.mean_dits(TimingClass::Dot),
                st.mean_dits(TimingClass::Dash)
            ),
            marks
        );
        assert!((st.mean_dits(TimingClass::ElementGap).unwrap() - 1.2).abs() < 1e-5);
        assert!((st.mean_dits(TimingClass::CharGap).unwrap() - 4.5).abs() < 1e-5);
        assert!((st.mean_dits(TimingClass::WordGap).unwrap() - 9.0).abs() < 1e-5);
        st.reset();
        assert_eq!(st.total_count(), 0);
        assert!(st.dash_dot_ratio().is_none());
    }

    /// MAN-315 D4: the sender word boundary and its clamp.
    #[test]
    fn sender_word_dits_rules() {
        let mut st = SenderTiming::new();
        assert_eq!(st.sender_word_dits(), None);
        st.observe(TimingClass::WordGap, 9.1, 1.0); // a word gap alone sets nothing
        assert_eq!(st.sender_word_dits(), None);
        let mut st = SenderTiming::new();
        st.observe(TimingClass::CharGap, 4.6, 1.0);
        let b = st.sender_word_dits().unwrap();
        assert!((b - 7.027).abs() < 0.01, "char only: {b}");
        st.observe(TimingClass::WordGap, 9.1, 1.0);
        let b = st.sender_word_dits().unwrap();
        assert!((b - 6.470).abs() < 0.01, "char + word: {b}");
        let mut st = SenderTiming::new();
        st.observe(TimingClass::CharGap, 1.5, 1.0);
        st.observe(TimingClass::WordGap, 1.6, 1.0);
        assert_eq!(st.sender_word_dits(), Some(3.0));
        let mut st = SenderTiming::new();
        st.observe(TimingClass::CharGap, 20.0, 1.0);
        st.observe(TimingClass::WordGap, 40.0, 1.0);
        assert_eq!(st.sender_word_dits(), Some(12.0));
    }

    #[test]
    fn pause_rule_scales_with_the_char_gap() {
        let mut st = SenderTiming::new();
        assert!(!st.is_pause(14.9));
        assert!(st.is_pause(15.0));
        st.observe(TimingClass::CharGap, 5.0, 1.0);
        assert!(!st.is_pause(19.9));
        assert!(st.is_pause(20.0));
    }

    /// MAN-315 D5: a weight-5 sender's dashes are not re-anchored to 3:1.
    #[test]
    fn ratio_ceiling_admits_heavy_dashes() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[48.0, 192.0, 48.0, 48.0, 192.0]);
        for _ in 0..40 {
            t.on_mark(240.0);
            t.on_mark(48.0);
        }
        let ratio = t.mu_dah_ms() / t.mu_dit_ms();
        assert!(ratio <= 6.0, "ratio {ratio}");
        assert!(t.mu_dah_ms() > 216.0, "mu_dah {}", t.mu_dah_ms());
    }

    /// Marks of `text` at `dit_ms` with dashes `weight` dits long, each
    /// mark alternately `jitter` (fractional) long and short.
    fn weighted_marks(text: &str, dit_ms: f32, weight: f32, jitter: f32) -> Vec<f32> {
        let mut out = Vec::new();
        for c in text.chars() {
            for e in crate::tree::pattern_for(c).unwrap().chars() {
                let base = if e == '.' { dit_ms } else { weight * dit_ms };
                let sign = if out.len() % 2 == 0 { 1.0 } else { -1.0 };
                out.push(base * (1.0 + sign * jitter));
            }
        }
        out
    }

    /// MAN-315 D5 (measured in planning): a 13 ms noise fragment seeds the
    /// dit cluster, real 48 ms dits join the dash cluster, and with the old
    /// 4.5 ceiling the 3:1 re-anchor kept the dit at 20 ms indefinitely.
    #[test]
    fn noise_seeded_init_heals_within_a_callsign_for_heavy_dashes() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[13.3, 50.7, 50.7, 53.3, 45.3]);
        let marks = weighted_marks("CQTESTK1ABCK1ABCTESTCQTEST", 48.0, 4.0, 0.08);
        let healed_at = marks.iter().position(|&m| {
            t.on_mark(m);
            (t.mu_dit_ms() - 48.0).abs() < 10.0 && (t.mu_dah_ms() / t.mu_dit_ms() - 4.0).abs() < 0.8
        });
        assert!(
            healed_at.is_some_and(|i| i < 60),
            "healed at {healed_at:?}: mu_dit {} mu_dah {}",
            t.mu_dit_ms(),
            t.mu_dah_ms()
        );
    }

    /// MAN-315 D5: the re-anchor target is the sender's ratio, not 3.0.
    #[test]
    fn ratio_hint_sets_reanchor_target() {
        for (hint, want) in [(None, 3.0f32), (Some(4.0), 4.0), (Some(9.0), 6.0)] {
            let mut t = SpeedTracker::new();
            feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
            t.set_ratio_hint(hint);
            let mut fired = None;
            for _ in 0..40 {
                let pre = t.mu_dah_ms();
                t.on_mark(115.0);
                if t.mu_dah_ms() > pre {
                    fired = Some(t.mu_dah_ms() / t.mu_dit_ms());
                    break;
                }
            }
            let r = fired.expect("collapse must trigger the ratio re-anchor");
            assert!((r - want).abs() < 1e-3, "hint {hint:?}: ratio {r}");
        }
    }

    #[test]
    fn drift_reinit_bumps_the_regime() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        assert_eq!(t.regime(), 0);
        for _ in 0..14 {
            t.on_mark(34.0);
        }
        assert_eq!(t.regime(), 1);
    }

    /// MAN-315 D4: the sender boundary replaces the nominal 5.0 but not an
    /// active Farnsworth pair.
    #[test]
    fn sender_word_dits_overrides_nominal_but_not_farnsworth() {
        let mut g = GapClassifier::new();
        let mu = 48.0;
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterWord);
        g.set_sender_word_dits(Some(7.0));
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(7.5 * mu, mu), GapClass::InterWord);
        assert_eq!(g.classify(1.5 * mu, mu), GapClass::InterElement);
        g.set_sender_word_dits(None);
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterWord);

        let mut g = GapClassifier::new();
        for _ in 0..5 {
            g.classify(6.0 * mu, mu);
            g.classify(14.0 * mu, mu);
        }
        g.set_sender_word_dits(Some(5.0));
        // Farnsworth active: the pair's ~9.2-dit boundary wins over 5.0.
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(14.0 * mu, mu), GapClass::InterWord);
    }

    #[test]
    fn timing_estimates_display_line() {
        let mut st = SenderTiming::new();
        st.observe(TimingClass::Dot, 1.0, 1.0);
        st.observe(TimingClass::Dot, 1.2, 1.0);
        let line = st.estimates(48.0).display_line();
        assert_eq!(
            line,
            "dot 1.03 ± 0.07  dash -  element gap -  char gap -  word gap -"
        );
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

    /// A mark/space boundary shift moves time from the mark to the gap or
    /// back; `(mu_dit + mu_egap)/2` must be invariant to it. Both signs are
    /// tested because a correctly-placed keying threshold (MAN-103) biases
    /// marks SHORT by the transmitter's rise time, while the old
    /// geometric-mean threshold biased them LONG (MAN-103 D6).
    #[test]
    fn dit_estimate_is_invariant_to_boundary_shift() {
        for delta in [-6.0f32, -2.0, 0.0, 2.0, 6.0] {
            let true_dit = 60.0;
            let mut t = SpeedTracker::new();
            for _ in 0..20 {
                t.on_mark(true_dit + delta);
                t.on_mark(3.0 * true_dit + delta);
                t.on_element_gap(true_dit - delta);
            }
            assert!(
                (t.dit_estimate_ms() - true_dit).abs() < 1.0,
                "delta {delta}: dit_estimate {}",
                t.dit_estimate_ms()
            );
            assert!(
                (t.wpm().unwrap() - 20.0).abs() < 0.5,
                "delta {delta}: wpm {:?}",
                t.wpm()
            );
        }
    }

    #[test]
    fn dit_estimate_falls_back_to_mu_dit_before_any_element_gap() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        assert!(t.mu_egap_ms().is_none());
        assert_eq!(t.dit_estimate_ms(), t.mu_dit_ms());
    }

    #[test]
    fn dit_estimate_correction_is_capped_two_sided() {
        // An absurdly short element gap should pull the estimate down by at
        // most DIT_BIAS_CAP_FRAC * mu_dit, never further, and never below
        // the SPEC §4.1 dit clamp floor.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        let mu_dit = t.mu_dit_ms();
        for _ in 0..20 {
            t.on_element_gap(0.001);
        }
        let floor = mu_dit - 0.35 * mu_dit;
        assert!(
            t.dit_estimate_ms() >= floor - 0.5,
            "dit_estimate {} below cap floor {floor}",
            t.dit_estimate_ms()
        );

        // An absurdly long element gap should pull it up by at most the
        // same cap, never further.
        let mut t2 = SpeedTracker::new();
        feed(&mut t2, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        let mu_dit2 = t2.mu_dit_ms();
        for _ in 0..20 {
            t2.on_element_gap(10_000.0);
        }
        let ceiling = mu_dit2 + 0.35 * mu_dit2;
        assert!(
            t2.dit_estimate_ms() <= ceiling + 0.5,
            "dit_estimate {} above cap ceiling {ceiling}",
            t2.dit_estimate_ms()
        );
    }

    #[test]
    fn drift_reinit_clears_the_gap_centroid() {
        // A stale mu_egap from the pre-drift regime must not leak its
        // correction into the post-reinit dit estimate.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        for _ in 0..3 {
            feed(&mut t, &[60.0, 180.0, 60.0]);
        }
        for _ in 0..20 {
            t.on_element_gap(60.0);
        }
        assert!(t.mu_egap_ms().is_some());
        for _ in 0..14 {
            t.on_mark(34.0); // fast dits: triggers the drift-regime reinit
        }
        assert!(
            t.mu_egap_ms().is_none(),
            "mu_egap must reset on drift reinit, got {:?}",
            t.mu_egap_ms()
        );
        assert_eq!(t.dit_estimate_ms(), t.mu_dit_ms());
    }

    #[test]
    fn mixed_streak_badlock_recovers() {
        // MAN-398: force the exact self-consistent bad lock -- a fabricated 30 ms
        // first sample isolates itself as the whole dit cluster, so mu_dit = 30 and
        // the boundary lands below every real element. check_drift's SPEC rule
        // is blind to this (the 12-mark streak's CV is ~0.47, over DRIFT_CV_MAX).
        // The CV-free lone-dit trigger catches it: the 30 ms first sample is the
        // lone dit and no element gap was fed, so mu_egap is None.
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
    fn homogeneous_badlock_recovers() {
        // MAN-6 on all-dit text ("HH"): the fabricated 30 ms first sample
        // isolates itself as the whole dit cluster and the real 84 ms dits
        // become mu_dah, so every dit reads as a dah. The 12-mark streak
        // sits on mu_dah (out of the SPEC drift rule's reach); the MAN-398
        // lone-dit branch must catch it.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[30.0, 84.0, 84.0, 84.0, 84.0]);
        assert!(
            t.boundary_ms() < 84.0,
            "bad lock not formed (boundary {})",
            t.boundary_ms()
        );
        feed(&mut t, &[84.0; 60]);
        assert!(
            (t.mu_dit_ms() - 84.0).abs() < 1.0,
            "mu_dit failed to recover: {}",
            t.mu_dit_ms()
        );
        assert!(
            t.boundary_ms() > 84.0,
            "real dits must classify as dits: boundary {}",
            t.boundary_ms()
        );
    }

    #[test]
    fn corroborated_singleton_dit_does_not_trigger_homogeneous_recovery() {
        // A singleton dit cluster that a later mark lands in is a real dit,
        // not a fabricated fragment: a following run of 12 dahs must not
        // re-split it.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[66.0, 200.0, 200.0, 200.0, 200.0]);
        t.on_mark(66.0);
        feed(&mut t, &[200.0; 12]);
        assert!(
            (t.mu_dit_ms() - 66.0).abs() < 1.0,
            "mu_dit should stay at 66, got {}",
            t.mu_dit_ms()
        );
        assert!(!t.take_badlock_recovered());
    }

    #[test]
    fn healthy_lock_survives_long_dah_run() {
        // A lock bootstrapped from several dits ("0", "MOM", "OOOO" all send
        // long dah runs) must not be touched by the homogeneous branch.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        feed(&mut t, &[180.0; 24]);
        assert!(
            (t.mu_dit_ms() - 60.0).abs() < 1.0,
            "mu_dit should stay at 60, got {}",
            t.mu_dit_ms()
        );
        assert!(!t.take_badlock_recovered());
    }

    /// Feed `marks`, each followed by an element gap of `egap_ms` (the decoder
    /// feeds `on_element_gap` only for gaps it classified `InterElement`).
    fn feed_with_egaps(t: &mut SpeedTracker, marks: &[f32], egap_ms: f32) {
        for &m in marks {
            t.on_mark(m);
            t.on_element_gap(egap_ms);
        }
    }

    #[test]
    fn fragment_lock_on_all_dah_text_is_kept() {
        // "MO" at 25 WPM: dit 48 ms, dah 144 ms (under DIT_CLAMP_MS.1). A 30 ms
        // fragment isolates itself as the dit cluster; the dahs are dahs and
        // their element gaps (48 ms) say so.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[30.0, 144.0, 144.0, 144.0, 144.0]);
        feed_with_egaps(&mut t, &[144.0; 48], 48.0);
        assert!(!t.take_badlock_recovered());
        assert!(
            (t.mu_dah_ms() - 144.0).abs() < 1.0,
            "mu_dah {}",
            t.mu_dah_ms()
        );
        assert!(t.boundary_ms() < 144.0, "boundary {}", t.boundary_ms());
    }

    #[test]
    fn genuine_opening_dit_then_dahs_is_kept() {
        // "E 0000" at 32 WPM: one real 37.5 ms dit, then 20 dahs of 112.5 ms
        // with 37.5 ms element gaps.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[37.5, 112.5, 112.5, 112.5, 112.5]);
        feed_with_egaps(&mut t, &[112.5; 20], 37.5);
        assert!(!t.take_badlock_recovered());
        assert!(
            (t.mu_dit_ms() - 37.5).abs() < 1.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!(
            (t.mu_dah_ms() - 112.5).abs() < 1.0,
            "mu_dah {}",
            t.mu_dah_ms()
        );
    }

    #[test]
    fn fragment_lock_on_dits_recovers_when_gaps_match_the_marks() {
        // "HH" at 22.6 WPM (dit 53 ms) with a 20 ms fragment: 53/20 = 2.65 >= 2.0
        // splits the bootstrap, so the fragment is the lone dit (lo_n = 1). A
        // 30 ms fragment (ratio 1.77) would take the unimodal init and never arm
        // the trigger. The element gaps are fed directly, so mu_egap is populated
        // and equals the marks (ratio 1.0), which makes them dits. This pins only
        // that the trigger fires on such a centroid; the dah-side comparison is
        // pinned by the two `_is_kept` tests above. In the decoder a 53 ms gap
        // reads as a character gap against the 20 ms dit and is never fed.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[20.0, 53.0, 53.0, 53.0, 53.0]);
        assert!(
            t.boundary_ms() < 53.0,
            "bad lock not formed (boundary {})",
            t.boundary_ms()
        );
        feed_with_egaps(&mut t, &[53.0; 11], 53.0);
        assert_eq!(t.mu_egap_ms(), Some(53.0));
        assert!(!t.take_badlock_recovered());
        // The 12th streak mark: the trigger evaluates with mu_egap populated.
        t.on_mark(53.0);
        assert!(
            t.take_badlock_recovered(),
            "trigger must fire on a populated gap centroid that matches the marks"
        );
        t.on_element_gap(53.0);
        feed_with_egaps(&mut t, &[53.0; 12], 53.0);
        assert!(
            (t.mu_dit_ms() - 53.0).abs() < 1.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!(t.boundary_ms() > 53.0, "boundary {}", t.boundary_ms());
    }

    #[test]
    fn mixed_fragment_lock_recovers_without_cv_gate() {
        // "5NN" at 17 WPM (dit 70 ms): every mark lands in the dah cluster, the
        // 12-mark streak mixes dits and dahs (CV ~0.6), and no element gap
        // classifies InterElement against the 20 ms fragment.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[20.0, 70.0, 70.0, 70.0, 70.0]);
        for _ in 0..4 {
            feed(
                &mut t,
                &[70.0, 70.0, 70.0, 70.0, 70.0, 210.0, 70.0, 210.0, 70.0],
            );
        }
        assert!(
            (t.mu_dit_ms() - 70.0).abs() < 5.0,
            "mu_dit {}",
            t.mu_dit_ms()
        );
        assert!(
            (t.mu_dah_ms() - 210.0).abs() < 15.0,
            "mu_dah {}",
            t.mu_dah_ms()
        );
    }

    #[test]
    fn corroborated_lock_is_not_resplit_by_a_bimodal_streak() {
        // A lock bootstrapped from three dits is corroborated. Twelve marks that
        // all land above its boundary and split 6/6 at ratio 2.27 (a fading
        // signal's stretched elements) must not re-seed it.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        for _ in 0..6 {
            feed(&mut t, &[110.0, 250.0]);
        }
        assert!(!t.take_badlock_recovered());
        assert!(t.mu_dit_ms() < 80.0, "mu_dit {}", t.mu_dit_ms());
    }

    #[test]
    fn gap_classification_nominal() {
        // CHAR_GAP_DITS boundary is SPEC §4.2's nominal 2.0 (MAN-103
        // retired the 1.6 deviation -- see
        // docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md and
        // docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md).
        let mut g = GapClassifier::new();
        let mu = 60.0;
        assert_eq!(g.classify(60.0, mu), GapClass::InterElement); // 1 dit
        assert_eq!(g.classify(180.0, mu), GapClass::InterChar); // 3 dits
        assert_eq!(g.classify(420.0, mu), GapClass::InterWord); // 7 dits
        assert_eq!(g.classify(108.0, mu), GapClass::InterElement); // 1.8 dits, < 2.0
        assert_eq!(g.classify(120.0, mu), GapClass::InterChar); // 2.0 dits, boundary
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

    /// MAN-103 D8: a gap the decoder resolved via the 7-dit safety-net
    /// flush (outside `classify`) must still advance the Farnsworth
    /// long-gap statistics, or the bootstrap can never complete once
    /// `mu_dit` runs at its corrected (shorter, non-inflated) value.
    #[test]
    fn observe_flushed_advances_farnsworth_bootstrap() {
        let mut g = GapClassifier::new();
        let mu = 48.0;
        for _ in 0..5 {
            g.observe_flushed(6.0 * mu, mu);
            g.observe_flushed(14.0 * mu, mu);
        }
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(14.0 * mu, mu), GapClass::InterWord);
    }

    /// MAN-213: closed one-character-word gaps rebuild the long-gap pair
    /// when they hold two cleanly split clusters of at least two gaps each,
    /// and a lone pause or irregular spacing among them does not.
    #[test]
    fn reinit_from_flushed_needs_two_gaps_per_cluster() {
        let mu = 48.0;
        let mut g = GapClassifier::new();
        assert!(!g.reinit_from_flushed(&[7.0, 7.0, 7.0, 7.0, 33.0], 7.0));
        // Irregular fragment spacing, and a split whose clusters sit closer
        // than FARNS_MIN_RATIO, do not rebuild either.
        assert!(!g.reinit_from_flushed(&[8.0, 9.0, 12.0, 20.0, 25.0], 7.0));
        assert!(!g.reinit_from_flushed(&[10.0, 10.0, 14.0, 15.0, 20.0], 7.0));
        assert_eq!(g.classify(7.0 * mu, mu), GapClass::InterWord);

        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[23.5, 55.0, 23.5, 55.0, 23.5], 7.0));
        assert_eq!(g.classify(23.5 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(55.0 * mu, mu), GapClass::InterWord);
        assert!(g.flush_threshold_dits(7.0) > 23.5);
    }

    /// MAN-264: two pauses after one-character words at ordinary spacing
    /// pass the rebuild's split check, with the 8-dit word gaps as the
    /// low cluster. The first ordinary character gap must undo it.
    #[test]
    fn rebuilt_pair_resets_on_an_ordinary_char_gap() {
        let mu = 48.0;
        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[8.0, 8.0, 33.0, 33.0, 8.0], 7.0));
        assert_eq!(g.classify(7.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(7.0 * mu, mu), GapClass::InterWord);

        // A genuine heavy-Farnsworth rebuild survives element gaps.
        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[23.5, 55.0, 23.5, 55.0, 23.5], 7.0));
        assert_eq!(g.classify(1.0 * mu, mu), GapClass::InterElement);
        assert_eq!(g.classify(23.5 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(55.0 * mu, mu), GapClass::InterWord);
    }

    /// MAN-264: once `FARNS_MIN_COUNT` word gaps have confirmed a rebuild,
    /// an ordinary character gap (a dit lost to a fade inside a Farnsworth
    /// character) no longer resets it. Ordinary 8-dit word gaps that
    /// `classify` sees after a false rebuild confirm nothing, so the next
    /// character gap still resets it.
    #[test]
    fn rebuilt_pair_stands_once_confirmed() {
        let mu = 48.0;
        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[23.5, 55.0, 23.5, 55.0, 23.5], 7.0));
        for _ in 0..FARNS_MIN_COUNT {
            assert_eq!(g.classify(23.5 * mu, mu), GapClass::InterChar);
            g.confirm_rebuilt(55.0 * mu, mu, None);
        }
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(23.5 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(55.0 * mu, mu), GapClass::InterWord);

        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[8.0, 8.0, 33.0, 33.0, 8.0], 7.0));
        for _ in 0..FARNS_MIN_COUNT {
            assert_eq!(g.classify(8.0 * mu, mu), GapClass::InterChar);
        }
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(7.0 * mu, mu), GapClass::InterWord);
    }

    /// MAN-264: with the flush configured below `WORD_GAP_DITS`, a
    /// Farnsworth character gap that outran it does not discard the
    /// rebuild it built, but an ordinary 3-dit character gap still discards
    /// a false one, even at or above a 3-dit flush.
    #[test]
    fn rebuilt_pair_disproof_follows_a_low_flush() {
        let mu = 48.0;
        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[4.9, 11.3, 4.9, 4.9, 11.3], 3.5));
        assert_eq!(g.classify(4.9 * mu, mu), GapClass::InterChar);
        assert!(g.flush_threshold_dits(3.5) > 4.9);

        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&[8.0, 8.0, 33.0, 33.0, 8.0], 3.0));
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.flush_threshold_dits(3.0), 3.0);
    }

    /// MAN-264: with the flush configured below `WORD_GAP_DITS`, a word
    /// gap the flush cut off at `flush_threshold_dits` sits below the
    /// rebuilt boundary, and must still confirm the rebuild. A closed gap
    /// below the boundary confirms nothing.
    #[test]
    fn rebuilt_pair_confirms_on_low_flushed_word_gaps() {
        let mu = 48.0;
        let us = [4.9, 11.3, 4.9, 4.9, 11.3];
        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&us, 3.5));
        let flushed = g.flush_threshold_dits(3.5);
        assert!(flushed < g.pair.boundary());
        for _ in 0..FARNS_MIN_COUNT {
            g.confirm_rebuilt(flushed * mu, mu, Some(3.5));
        }
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert!(g.flush_threshold_dits(3.5) > 4.9);

        let mut g = GapClassifier::new();
        assert!(g.reinit_from_flushed(&us, 3.5));
        for _ in 0..FARNS_MIN_COUNT {
            g.confirm_rebuilt(flushed * mu, mu, None);
        }
        assert_eq!(g.classify(3.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.flush_threshold_dits(3.5), 3.5);
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
