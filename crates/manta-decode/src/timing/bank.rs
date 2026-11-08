//! MAN-544: a bounded bank of speed/timing hypotheses (decoder-recall
//! research §7, `docs/DECISIONS/2026-09-09-decoder-recall-research.md`).
//!
//! The decoder's `SpeedTracker` stays the primary hypothesis. The bank holds
//! up to `width - 1` challenger trackers spawned from evidence (an ambiguous
//! bootstrap, or a streak of marks on one side of the primary's boundary),
//! scores every hypothesis on every mark and gap with a max-component
//! log-normal fit, and names a challenger as the new primary only when it
//! leads by `SWITCH_LEAD_NATS` on `SWITCH_CONFIRM_MARKS` consecutive marks
//! and its gap score is not behind the primary's (the gap veto: marks
//! alone never switch). With `switching: false` it only records the
//! switches it would make. `width: 1` is the single-tracker decode: every
//! method returns at its first line.

use super::{ClusterPair, SpeedTracker, CHAR_GAP_DITS, DIT_CLAMP_MS};
use std::collections::VecDeque;

/// Marks on one side of the primary's boundary that spawn a challenger.
const SPAWN_STREAK: usize = 4;
/// Log-domain sigma of a mark around its class centroid.
const SIGMA_MARK: f32 = 0.25;
/// Log-domain sigma of a gap around 1, 3 or 7 units.
const SIGMA_GAP: f32 = 0.35;
/// Per-observation floor of the log-likelihood, in nats: one fade
/// fragment cannot cost a hypothesis more than this.
const FLOOR: f32 = 3.0;
/// Score decay per observation.
const DECAY: f32 = 0.90;
/// Lead over the primary, in nats, a challenger must hold to switch.
const SWITCH_LEAD_NATS: f32 = 4.0;
/// Consecutive marks the lead must hold, and the least age to switch.
const SWITCH_CONFIRM_MARKS: u32 = 2;
/// A gap of at least this many units of the slowest hypothesis is a pause
/// and is scored by none.
const PAUSE_DITS: f32 = 12.0;
/// Two hypotheses closer than this on both `ln lo` and `ln hi` are one.
const DUP_LOG_RATIO: f32 = 0.25;
/// A challenger younger than this is never replaced by a new one.
const MIN_AGE_MARKS: u32 = 8;
/// Marks a former primary is kept after a switch, whatever its score.
const RETAIN_MARKS: u32 = 24;
/// A challenger trailing the primary by this much is dropped.
const PRUNE_NATS: f32 = 12.0;
/// A challenger that has not switched in this many marks is dropped.
const MAX_AGE_MARKS: u32 = 64;
/// Switch records kept (a bounded ring).
const SWITCH_LOG_LEN: usize = 16;

/// MAN-544: tunables of the bounded speed/timing hypothesis bank. Not a
/// TOML option (as `steered_frontend`): `width: 1` is the single-tracker
/// decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedBankConfig {
    /// Hypotheses kept, the primary included. 1 = off.
    pub width: usize,
    /// false: score and record would-be switches, never swap (observe-only).
    pub switching: bool,
}

impl Default for SpeedBankConfig {
    /// Off (`width: 1`) until the switching gates pass (MAN-544 plan,
    /// Phases 3-4).
    fn default() -> Self {
        SpeedBankConfig {
            width: 1,
            switching: false,
        }
    }
}

/// One switch the bank made (`applied`) or would have made (observe-only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwitchRecord {
    /// Marks the bank had seen when it decided (1-based).
    pub mark_index: u64,
    /// The primary's `mu_dit_ms` before the switch.
    pub from_dit_ms: f32,
    /// The challenger's `mu_dit_ms`.
    pub to_dit_ms: f32,
    pub applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Bootstrap,
    Streak,
    /// A primary a switch replaced, kept for `retain_marks` more marks.
    FormerPrimary {
        retain_marks: u32,
    },
}

/// One challenger: its tracker, its decayed scores and its bookkeeping.
#[derive(Debug, Clone)]
pub struct TimingHypothesis {
    tracker: SpeedTracker,
    /// Decayed log-likelihood over marks and gaps.
    score: f32,
    /// Decayed gap-only part of `score`, for the gap veto.
    gap_score: f32,
    /// Consecutive marks leading the primary by `SWITCH_LEAD_NATS`.
    lead_marks: u32,
    /// Marks since spawn.
    age_marks: u32,
    origin: Origin,
    /// Observe-only: the would-switch of this lead is already recorded.
    reported: bool,
    ln_lo: f32,
    ln_hi: f32,
}

impl TimingHypothesis {
    fn new(tracker: SpeedTracker, origin: Origin, score: f32, gap_score: f32) -> Self {
        let (ln_lo, ln_hi) = ln_centroids(&tracker);
        TimingHypothesis {
            tracker,
            score,
            gap_score,
            lead_marks: 0,
            age_marks: 0,
            origin,
            reported: false,
            ln_lo,
            ln_hi,
        }
    }

    /// The challenger's tracker.
    pub fn tracker(&self) -> &SpeedTracker {
        &self.tracker
    }

    fn retained(&self) -> bool {
        matches!(self.origin, Origin::FormerPrimary { retain_marks } if retain_marks > 0)
    }
}

/// MAN-544: the bounded bank. See the module doc.
#[derive(Debug, Clone)]
pub struct TimingBank {
    cfg: SpeedBankConfig,
    primary_score: f32,
    primary_gap_score: f32,
    challengers: Vec<TimingHypothesis>,
    streak: Vec<f32>,
    /// Element-gap candidates between the streak's marks.
    streak_gaps: Vec<f32>,
    streak_side: Option<bool>,
    /// The latest scored gap, consumed by the next mark's streak step.
    last_gap: Option<f32>,
    marks_seen: u64,
    /// Index of a challenger that qualified on the latest mark.
    pending: Option<usize>,
    log: VecDeque<SwitchRecord>,
}

fn ln_centroids(t: &SpeedTracker) -> (f32, f32) {
    (t.mu_dit_ms().max(1e-3).ln(), t.mu_dah_ms().max(1e-3).ln())
}

/// Max-component log-normal log-likelihood of `x_ln` over `centers`,
/// floored at `-FLOOR`.
#[inline]
fn class_ll(x_ln: f32, centers: &[f32], sigma: f32) -> f32 {
    let mut best = -FLOOR;
    for &c in centers {
        let z = (x_ln - c) / sigma;
        best = best.max(-0.5 * z * z);
    }
    best
}

#[inline]
fn mark_ll(x_ln: f32, ln_lo: f32, ln_hi: f32) -> f32 {
    class_ll(x_ln, &[ln_lo, ln_hi], SIGMA_MARK)
}

const LN_3: f32 = 1.098_612_3;
const LN_7: f32 = 1.945_910_1;

#[inline]
fn gap_ll(x_ln: f32, unit_ms: f32) -> f32 {
    let u = unit_ms.max(1e-3).ln();
    class_ll(x_ln, &[u, u + LN_3, u + LN_7], SIGMA_GAP)
}

fn duplicates(a: (f32, f32), b: (f32, f32)) -> bool {
    (a.0 - b.0).abs() < DUP_LOG_RATIO && (a.1 - b.1).abs() < DUP_LOG_RATIO
}

impl SpeedTracker {
    /// MAN-544: a ready tracker re-initialized from `marks` (the ordinary
    /// `ClusterPair::initialize` split, so a unimodal set follows its
    /// direction rule), with `element_gaps` seeding `mu_egap_ms`.
    fn from_marks(marks: &[f32], element_gaps: &[f32]) -> SpeedTracker {
        let mut t = SpeedTracker::new();
        t.pair.reinit_from(marks);
        t.apply_constraints();
        for &g in element_gaps {
            if g < CHAR_GAP_DITS * t.dit_estimate_ms() {
                t.on_element_gap(g);
            }
        }
        t.wpm_ema = Some(1200.0 / t.dit_estimate_ms());
        t
    }

    /// MAN-544: the other reading of an unconfirmed unimodal lock: a
    /// dit-assumed `(m, 3m)` becomes dah-assumed `(m/3, m)` and the reverse.
    /// `None` when the lock is confirmed or the new dit would leave
    /// `DIT_CLAMP_MS`.
    fn three_to_one_alternative(&self) -> Option<SpeedTracker> {
        let p = &self.pair;
        if !p.ready || p.confirmed {
            return None;
        }
        let (lo, hi, placeholder_is_lo) = if p.placeholder_is_lo {
            (p.hi, 3.0 * p.hi, false)
        } else {
            (p.lo / 3.0, p.lo, true)
        };
        if !(DIT_CLAMP_MS.0..=DIT_CLAMP_MS.1).contains(&lo) {
            return None;
        }
        let mut t = SpeedTracker::new();
        t.pair = ClusterPair {
            lo,
            hi,
            init: Vec::new(),
            ready: true,
            confirmed: false,
            placeholder_is_lo,
            unimodal_ceiling: p.unimodal_ceiling,
            lo_n: 0,
        };
        t.apply_constraints();
        t.wpm_ema = Some(1200.0 / t.dit_estimate_ms());
        Some(t)
    }
}

impl TimingBank {
    pub fn new(cfg: SpeedBankConfig) -> Self {
        let width = cfg.width.max(1);
        let cap = if width > 1 { SPAWN_STREAK } else { 0 };
        TimingBank {
            cfg: SpeedBankConfig { width, ..cfg },
            primary_score: 0.0,
            primary_gap_score: 0.0,
            challengers: Vec::with_capacity(width - 1),
            streak: Vec::with_capacity(cap),
            streak_gaps: Vec::with_capacity(cap),
            streak_side: None,
            last_gap: None,
            marks_seen: 0,
            pending: None,
            log: VecDeque::new(),
        }
    }

    fn off(&self) -> bool {
        self.cfg.width <= 1
    }

    /// Hypotheses alive, the primary included.
    pub fn hypotheses(&self) -> usize {
        1 + self.challengers.len()
    }

    /// Marks scored since the bank was built (`SwitchRecord::mark_index`'s
    /// clock).
    pub fn marks_seen(&self) -> u64 {
        self.marks_seen
    }

    /// The challengers alive, in spawn order.
    pub fn challengers(&self) -> &[TimingHypothesis] {
        &self.challengers
    }

    /// The latest switch records, oldest first (at most 16).
    pub fn switch_log(&self) -> impl Iterator<Item = &SwitchRecord> {
        self.log.iter()
    }

    /// Drop every challenger and all evidence (an unknown input gap, a
    /// re-seeded tracker).
    pub fn clear(&mut self) {
        if self.off() {
            return;
        }
        self.challengers.clear();
        self.primary_score = 0.0;
        self.primary_gap_score = 0.0;
        self.streak.clear();
        self.streak_gaps.clear();
        self.streak_side = None;
        self.last_gap = None;
        self.pending = None;
    }

    /// The primary just became ready from `bootstrap_marks`: spawn the other
    /// reading of an ambiguous bootstrap. A unimodal dit-assumed lock gets
    /// its dah-assumed twin and a dah-assumed (MAN-402-flipped) one its
    /// dit-assumed twin; a bimodal split whose dit cluster is one mark (the
    /// MAN-398 fragment shape) gets a tracker built from the other marks.
    pub fn on_ready(&mut self, primary: &SpeedTracker, bootstrap_marks: &[f32]) {
        if self.off() {
            return;
        }
        let candidate = if !primary.pair.confirmed {
            primary.three_to_one_alternative()
        } else if primary.pair.lo_n == 1 && bootstrap_marks.len() >= 3 {
            let min_i = bootstrap_marks
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .unwrap_or(0);
            let rest: Vec<f32> = bootstrap_marks
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != min_i)
                .map(|(_, &m)| m)
                .collect();
            Some(SpeedTracker::from_marks(&rest, &[]))
        } else {
            None
        };
        if let Some(t) = candidate {
            self.spawn(primary, t, Origin::Bootstrap);
        }
    }

    /// Score one gap that reached classification, against every hypothesis.
    /// A pause (at least `PAUSE_DITS` units of the slowest hypothesis) is
    /// scored by none.
    pub fn on_gap(&mut self, primary: &SpeedTracker, dur_ms: f32) {
        if self.off() || !primary.ready() || !dur_ms.is_finite() || dur_ms <= 0.0 {
            return;
        }
        let u_max = self
            .challengers
            .iter()
            .map(|c| c.tracker.dit_estimate_ms())
            .fold(primary.dit_estimate_ms(), f32::max);
        if dur_ms >= PAUSE_DITS * u_max {
            self.last_gap = None;
            return;
        }
        let x = dur_ms.ln();
        let l = gap_ll(x, primary.dit_estimate_ms());
        self.primary_score = DECAY * self.primary_score + l;
        self.primary_gap_score = DECAY * self.primary_gap_score + l;
        for c in &mut self.challengers {
            let unit = c.tracker.dit_estimate_ms();
            let l = gap_ll(x, unit);
            c.score = DECAY * c.score + l;
            c.gap_score = DECAY * c.gap_score + l;
            if dur_ms < CHAR_GAP_DITS * unit {
                c.tracker.on_element_gap(dur_ms);
            }
        }
        self.last_gap = Some(dur_ms);
    }

    /// Score one mark against every hypothesis, after the primary has been
    /// fed it. `live` marks also update the challengers and the streak; a
    /// replayed warm-up mark (already in every tracker's bootstrap) is only
    /// scored.
    pub fn on_mark(&mut self, primary: &SpeedTracker, dur_ms: f32, live: bool) {
        if self.off() || !primary.ready() || !dur_ms.is_finite() || dur_ms <= 0.0 {
            return;
        }
        self.marks_seen += 1;
        let x = dur_ms.ln();
        let (p_lo, p_hi) = ln_centroids(primary);
        self.primary_score = DECAY * self.primary_score + mark_ll(x, p_lo, p_hi);
        for c in &mut self.challengers {
            c.score = DECAY * c.score + mark_ll(x, c.ln_lo, c.ln_hi);
            if live {
                c.tracker.on_mark(dur_ms);
                // The bank owns a challenger's re-seeds; the decoder resets
                // its gap state on a swap-in, not on this flag.
                c.tracker.take_badlock_recovered();
                let (lo, hi) = ln_centroids(&c.tracker);
                c.ln_lo = lo;
                c.ln_hi = hi;
            }
            c.age_marks = c.age_marks.saturating_add(1);
            if let Origin::FormerPrimary { retain_marks } = &mut c.origin {
                *retain_marks = retain_marks.saturating_sub(1);
            }
        }
        let gap = self.last_gap.take();
        if live {
            self.step_streak(primary, dur_ms, gap);
        }
        self.prune(primary);
        self.pending = self.qualifying();
    }

    fn step_streak(&mut self, primary: &SpeedTracker, dur_ms: f32, gap: Option<f32>) {
        let side = dur_ms >= primary.boundary_ms();
        if self.streak_side == Some(side) && self.streak.len() < SPAWN_STREAK {
            if let Some(g) = gap {
                self.streak_gaps.push(g);
            }
            self.streak.push(dur_ms);
        } else {
            self.streak.clear();
            self.streak_gaps.clear();
            self.streak.push(dur_ms);
            self.streak_side = Some(side);
        }
        if self.streak.len() < SPAWN_STREAK {
            return;
        }
        let mut candidate = SpeedTracker::from_marks(&self.streak, &self.streak_gaps);
        if duplicates(ln_centroids(&candidate), ln_centroids(primary)) {
            match candidate.three_to_one_alternative() {
                Some(alt) => candidate = alt,
                None => {
                    self.streak.clear();
                    self.streak_gaps.clear();
                    return;
                }
            }
        }
        self.streak.clear();
        self.streak_gaps.clear();
        self.spawn(primary, candidate, Origin::Streak);
    }

    /// Admit `t` as a challenger unless it duplicates a live hypothesis;
    /// when full, it replaces the lowest-scoring challenger old enough and
    /// not retained, else it is dropped.
    fn spawn(&mut self, primary: &SpeedTracker, t: SpeedTracker, origin: Origin) {
        let ln = ln_centroids(&t);
        if duplicates(ln, ln_centroids(primary))
            || self
                .challengers
                .iter()
                .any(|c| duplicates(ln, (c.ln_lo, c.ln_hi)))
        {
            return;
        }
        let h = TimingHypothesis::new(t, origin, self.primary_score, self.primary_gap_score);
        if self.challengers.len() + 1 < self.cfg.width {
            self.challengers.push(h);
            return;
        }
        let victim = self
            .challengers
            .iter()
            .enumerate()
            .filter(|(_, c)| c.age_marks >= MIN_AGE_MARKS && !c.retained())
            .min_by(|a, b| a.1.score.total_cmp(&b.1.score))
            .map(|(i, _)| i);
        if let Some(i) = victim {
            self.challengers[i] = h;
        }
    }

    fn prune(&mut self, primary: &SpeedTracker) {
        let p = ln_centroids(primary);
        let primary_score = self.primary_score;
        self.challengers.retain(|c| {
            if duplicates((c.ln_lo, c.ln_hi), p) {
                return false;
            }
            if c.retained() {
                return true;
            }
            primary_score - c.score < PRUNE_NATS && c.age_marks < MAX_AGE_MARKS
        });
    }

    /// Update each challenger's lead streak; return the best challenger
    /// that qualifies to switch (highest score, then lowest index).
    fn qualifying(&mut self) -> Option<usize> {
        let primary_score = self.primary_score;
        let primary_gap_score = self.primary_gap_score;
        let mut best: Option<(usize, f32)> = None;
        for (i, c) in self.challengers.iter_mut().enumerate() {
            if c.score - primary_score >= SWITCH_LEAD_NATS {
                c.lead_marks = c.lead_marks.saturating_add(1);
            } else {
                c.lead_marks = 0;
                c.reported = false;
            }
            let qualifies = c.lead_marks >= SWITCH_CONFIRM_MARKS
                && c.age_marks >= SWITCH_CONFIRM_MARKS
                && c.gap_score >= primary_gap_score;
            if qualifies && best.is_none_or(|(_, s)| c.score > s) {
                best = Some((i, c.score));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Act on a challenger that qualified on the latest mark. With
    /// switching on, swap it into `primary` (the old primary stays as a
    /// retained challenger) and return true; observe-only, record the
    /// would-switch once per lead and return false.
    pub fn take_switch(&mut self, primary: &mut SpeedTracker) -> bool {
        if self.off() {
            return false;
        }
        let Some(i) = self.pending.take() else {
            return false;
        };
        let c = &mut self.challengers[i];
        if !self.cfg.switching {
            if !c.reported {
                c.reported = true;
                let rec = SwitchRecord {
                    mark_index: self.marks_seen,
                    from_dit_ms: primary.mu_dit_ms(),
                    to_dit_ms: c.tracker.mu_dit_ms(),
                    applied: false,
                };
                self.push_log(rec);
            }
            return false;
        }
        let rec = SwitchRecord {
            mark_index: self.marks_seen,
            from_dit_ms: primary.mu_dit_ms(),
            to_dit_ms: c.tracker.mu_dit_ms(),
            applied: true,
        };
        std::mem::swap(primary, &mut c.tracker);
        std::mem::swap(&mut self.primary_score, &mut c.score);
        std::mem::swap(&mut self.primary_gap_score, &mut c.gap_score);
        primary.take_badlock_recovered();
        c.origin = Origin::FormerPrimary {
            retain_marks: RETAIN_MARKS,
        };
        c.lead_marks = 0;
        c.age_marks = 0;
        c.reported = false;
        let (lo, hi) = ln_centroids(&c.tracker);
        c.ln_lo = lo;
        c.ln_hi = hi;
        self.streak.clear();
        self.streak_gaps.clear();
        self.streak_side = None;
        self.push_log(rec);
        true
    }

    fn push_log(&mut self, rec: SwitchRecord) {
        if self.log.len() == SWITCH_LOG_LEN {
            self.log.pop_front();
        }
        self.log.push_back(rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVE: SpeedBankConfig = SpeedBankConfig {
        width: 3,
        switching: false,
    };
    const SWITCHING: SpeedBankConfig = SpeedBankConfig {
        width: 3,
        switching: true,
    };

    /// Drive a primary and a bank like `TrackDecoder` does: marks feed
    /// the primary first, gaps are scored before classification, and the
    /// bank's switch is taken after every mark.
    struct Rig {
        primary: SpeedTracker,
        bank: TimingBank,
        warm: Vec<f32>,
    }

    impl Rig {
        fn new(cfg: SpeedBankConfig) -> Self {
            Rig {
                primary: SpeedTracker::new(),
                bank: TimingBank::new(cfg),
                warm: Vec::new(),
            }
        }

        fn mark(&mut self, ms: f32) {
            if !self.primary.ready() {
                self.primary.on_mark(ms);
                self.warm.push(ms);
                if self.primary.ready() {
                    let warm = std::mem::take(&mut self.warm);
                    self.bank.on_ready(&self.primary, &warm);
                }
                return;
            }
            self.primary.on_mark(ms);
            self.bank.on_mark(&self.primary, ms, true);
            self.bank.take_switch(&mut self.primary);
        }

        fn gap(&mut self, ms: f32) {
            self.bank.on_gap(&self.primary, ms);
            if self.primary.ready() && ms < CHAR_GAP_DITS * self.primary.dit_estimate_ms() {
                self.primary.on_element_gap(ms);
            }
        }

        /// Key `text` at `dit` ms with `egap`/`cgap`/`wgap` ms spaces.
        fn text(&mut self, text: &str, dit: f32, dah: f32, egap: f32, cgap: f32, wgap: f32) {
            for (wi, word) in text.split(' ').enumerate() {
                if wi > 0 {
                    self.gap(wgap);
                }
                for (ci, ch) in word.chars().enumerate() {
                    if ci > 0 {
                        self.gap(cgap);
                    }
                    let pat = crate::tree::pattern_for(ch).unwrap();
                    for (ei, e) in pat.chars().enumerate() {
                        if ei > 0 {
                            self.gap(egap);
                        }
                        self.mark(if e == '-' { dah } else { dit });
                    }
                }
            }
            self.gap(wgap);
        }

        fn std_text(&mut self, text: &str, dit: f32) {
            self.text(text, dit, 3.0 * dit, dit, 3.0 * dit, 7.0 * dit);
        }

        fn log(&self) -> Vec<SwitchRecord> {
            self.bank.switch_log().copied().collect()
        }
    }

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn within(x: f32, want: f32, frac: f32) -> bool {
        (x / want - 1.0).abs() <= frac
    }

    #[test]
    fn width_one_never_spawns() {
        let mut r = Rig::new(SpeedBankConfig {
            width: 1,
            switching: true,
        });
        for _ in 0..10 {
            r.std_text("CQ TEST W1AW", 60.0);
        }
        for _ in 0..20 {
            r.std_text("CQ TEST W1AW", 20.0);
        }
        assert_eq!(r.bank.hypotheses(), 1);
        assert!(r.log().is_empty());
    }

    #[test]
    fn hypotheses_never_exceed_width() {
        for width in [2usize, 3, 4] {
            let mut r = Rig::new(SpeedBankConfig {
                width,
                switching: true,
            });
            let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
            for _ in 0..10_000 {
                let m = 15.0 + (rng.next() % 400) as f32;
                r.mark(m);
                let g = 10.0 + (rng.next() % 600) as f32;
                r.gap(g);
                assert!(r.bank.hypotheses() <= width, "width {width}");
            }
        }
    }

    /// Lock at `from` ms dits on CQ-like text, then key `to` ms dits; returns
    /// the rig and the mark count at the step.
    fn step(cfg: SpeedBankConfig, from: f32, to: f32) -> (Rig, u64) {
        let mut r = Rig::new(cfg);
        for _ in 0..3 {
            r.std_text("CQ TEST", from);
        }
        let at = r.bank.marks_seen;
        for _ in 0..4 {
            r.std_text("CQ TEST", to);
        }
        (r, at)
    }

    #[test]
    fn two_x_up_step_would_switch_within_eight_marks() {
        let (r, at) = step(OBSERVE, 60.0, 30.0);
        let log = r.log();
        let hit = log
            .iter()
            .find(|s| !s.applied && within(s.to_dit_ms, 30.0, 0.2))
            .unwrap_or_else(|| panic!("no would-switch to 30 ms: {log:?}"));
        assert!(hit.mark_index <= at + 8, "{hit:?} step at {at}");
    }

    #[test]
    fn half_speed_down_step_would_switch_within_eight_marks() {
        let (r, at) = step(OBSERVE, 30.0, 60.0);
        let log = r.log();
        let hit = log
            .iter()
            .find(|s| !s.applied && within(s.to_dit_ms, 60.0, 0.2))
            .unwrap_or_else(|| panic!("no would-switch to 60 ms: {log:?}"));
        assert!(hit.mark_index <= at + 8, "{hit:?} step at {at}");
    }

    /// `timing.rs`'s `corroborated_lock_is_not_resplit_by_a_bimodal_streak`
    /// marks: a 60/180 lock, then six stretched [110, 250] pairs.
    fn fade_rig(egap: f32) -> Rig {
        let mut r = Rig::new(OBSERVE);
        for (i, m) in [60.0, 180.0, 60.0, 60.0, 180.0].into_iter().enumerate() {
            if i > 0 {
                r.gap(60.0);
            }
            r.mark(m);
        }
        r.gap(180.0);
        for i in 0..6 {
            r.mark(110.0);
            r.gap(egap + (i % 2) as f32 * 10.0);
            r.mark(250.0);
            r.gap(egap);
        }
        r
    }

    #[test]
    fn fade_stretched_streak_with_old_scale_gaps_never_switches() {
        let r = fade_rig(30.0);
        assert!(r.log().is_empty(), "{:?}", r.log());
    }

    #[test]
    fn genuine_slow_down_with_stretched_gaps_would_switch() {
        let r = fade_rig(110.0);
        let log = r.log();
        assert!(
            log.iter().any(|s| within(s.to_dit_ms, 110.0, 0.2)),
            "{log:?}"
        );
    }

    /// The plan's `HH` stream is the `TT`/`I` ambiguity: with no dah, every
    /// 60 ms mark is a dah of a 20 ms lock and every 60 ms element gap its
    /// character gap, so only word gaps separate the readings. `5NN` (the
    /// grid's dit-heavy text with dahs) is the stream marks can resolve.
    #[test]
    fn fragment_lock_on_dit_text_would_switch_within_ten_marks() {
        let mut r = Rig::new(OBSERVE);
        for (i, m) in [14.0, 60.0, 60.0, 60.0, 180.0].into_iter().enumerate() {
            if i > 0 {
                r.gap(60.0);
            }
            r.mark(m);
        }
        assert!(r.primary.mu_dit_ms() < 30.0, "fragment-seeded lock");
        assert_eq!(r.bank.hypotheses(), 2, "bootstrap twin spawned");
        r.gap(180.0);
        let at = r.bank.marks_seen;
        for _ in 0..4 {
            r.std_text("5NN", 60.0);
        }
        let log = r.log();
        let hit = log
            .iter()
            .find(|s| within(s.to_dit_ms, 60.0, 0.2))
            .unwrap_or_else(|| panic!("no would-switch to 60 ms: {log:?}"));
        assert!(hit.mark_index <= at + 10, "{hit:?} from {at}");
    }

    #[test]
    fn correct_dah_lock_on_mo_is_kept() {
        let mut r = Rig::new(OBSERVE);
        for _ in 0..5 {
            r.primary.on_mark(109.0);
        }
        assert!(r.primary.resolve_unimodal_bootstrap(&[36.0, 36.0, 36.0]));
        assert!(within(r.primary.mu_dit_ms(), 36.3, 0.05));
        r.bank.on_ready(&r.primary, &[109.0; 5]);
        assert_eq!(r.bank.hypotheses(), 2, "dit-assumed twin spawned");
        for _ in 0..10 {
            r.text("MO", 36.0, 109.0, 36.0, 109.0, 254.0);
        }
        assert!(r.log().is_empty(), "{:?}", r.log());
    }

    #[test]
    fn homogeneous_dahs_without_element_gaps_never_switch() {
        let mut r = Rig::new(OBSERVE);
        for _ in 0..3 {
            r.std_text("CQ TEST", 40.0);
        }
        for _ in 0..20 {
            r.std_text("TTT", 40.0);
        }
        assert!(r.log().is_empty(), "{:?}", r.log());
        assert!(within(r.primary.mu_dit_ms(), 40.0, 0.2));
    }

    #[test]
    fn isolated_fragments_do_not_switch() {
        let mut r = Rig::new(OBSERVE);
        let mut n = 0u32;
        for _ in 0..20 {
            for ch in ['C', 'Q', 'T', 'E', 'S', 'T'] {
                for e in crate::tree::pattern_for(ch).unwrap().chars() {
                    n += 1;
                    let m = if n.is_multiple_of(7) {
                        10.0
                    } else if e == '-' {
                        144.0
                    } else {
                        48.0
                    };
                    r.mark(m);
                    r.gap(48.0);
                }
                r.gap(144.0);
            }
        }
        assert!(r.log().is_empty(), "{:?}", r.log());
    }

    #[test]
    fn pauses_do_not_move_the_score_difference() {
        let (mut r, _) = step(OBSERVE, 60.0, 30.0);
        let diffs = |b: &TimingBank| -> Vec<f32> {
            b.challengers
                .iter()
                .map(|c| c.score - b.primary_score)
                .collect()
        };
        let before = diffs(&r.bank);
        r.gap(2000.0);
        assert_eq!(before, diffs(&r.bank));
    }

    #[test]
    fn decisions_are_deterministic_and_ties_break_by_spawn_order() {
        let run = || {
            let (r, _) = step(OBSERVE, 60.0, 30.0);
            r.log()
        };
        let a = run();
        assert!(!a.is_empty());
        assert_eq!(a, run());
        // Equal scores keep the lower index.
        let mut b = TimingBank::new(OBSERVE);
        let t = SpeedTracker::from_marks(&[30.0, 90.0, 30.0, 30.0], &[]);
        for _ in 0..2 {
            let mut h = TimingHypothesis::new(t.clone(), Origin::Streak, 10.0, 0.0);
            h.lead_marks = SWITCH_CONFIRM_MARKS;
            h.age_marks = SWITCH_CONFIRM_MARKS;
            b.challengers.push(h);
        }
        assert_eq!(b.qualifying(), Some(0));
    }

    #[test]
    fn former_primary_switches_back_after_a_false_switch() {
        let mut r = Rig::new(SWITCHING);
        for _ in 0..3 {
            r.std_text("CQ TEST", 60.0);
        }
        r.std_text("CQ TEST", 30.0);
        let first = r.log();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].applied && within(first[0].to_dit_ms, 30.0, 0.2));
        for _ in 0..3 {
            r.std_text("CQ TEST", 60.0);
        }
        let log = r.log();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[1].applied, "{log:?}");
        // The former primary absorbed the burst's marks (its dit cluster
        // drifted toward the 90 ms dahs), so it returns near, not at, 60 ms
        // and settles there.
        assert!(within(log[1].to_dit_ms, 60.0, 0.25), "{log:?}");
        assert!(
            log[1].mark_index - log[0].mark_index <= RETAIN_MARKS as u64,
            "{log:?}"
        );
        assert!(within(r.primary.mu_dit_ms(), 60.0, 0.1));
    }
}
