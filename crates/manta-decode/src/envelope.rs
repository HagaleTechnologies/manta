//! Per-track demodulation: normalization, dual-EMA keying threshold,
//! hysteresis + debounce, element stream. SPEC §3.

use crate::{ms_to_hops, HOP_MS};
use std::collections::VecDeque;

/// SPEC §9 [decode] table defaults.
#[derive(Debug, Clone)]
pub struct DemodConfig {
    pub hyst_up: f32,
    pub hyst_down: f32,
    pub debounce_ms: f64,
    pub tau_lo_ms: f64,
    pub tau_hi_init_ms: f64,
    pub tau_hi_bounds_ms: (f64, f64),
}

impl Default for DemodConfig {
    fn default() -> Self {
        DemodConfig {
            hyst_up: 1.25,
            hyst_down: 0.80,
            debounce_ms: 12.0,
            tau_lo_ms: 500.0,
            tau_hi_init_ms: 200.0,
            tau_hi_bounds_ms: (100.0, 400.0),
        }
    }
}

/// A completed mark or space run at 375 Hz. SPEC §3.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub mark: bool,
    /// Input-stream sample counter of the run's leading edge. SPEC §3.4.
    pub start_ts: u64,
    pub hops: u32,
}

pub(crate) fn alpha_from_tau_ms(tau_ms: f64) -> f32 {
    (1.0 - (-HOP_MS / tau_ms).exp()) as f32
}

/// Nearest-rank quantile on a sorted slice (pinned decision 6).
fn quantile_nearest_rank(sorted: &[f32], q: f64) -> f32 {
    let n = sorted.len();
    let idx = ((q * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

const INIT_HOPS: usize = 375; // SPEC §3.2: rails from the first 1 s
const AREF_HOPS: usize = 188; // SPEC §3.1: A_ref from the first 500 ms (ms_to_hops(500))
const MIN_KEYING_RATIO: f32 = 2.0; // SPEC §3.2: < 6 dB apparent depth -> pre-decode
const E_LO_FLOOR: f32 = 1e-6;
const SNR_BW_CORR_DB: f32 = 14.3; // SPEC §2.3: 10*log10(2500/93.75)

enum Phase {
    /// Collecting the first INIT_HOPS; retries every INIT_HOPS on failure
    /// (pinned decision 10).
    Init {
        buf: Vec<(f32, u64)>,
        hops_until_attempt: usize,
    },
    Running,
}

/// Per-track demodulator: raw envelope samples in, keyed mark/space runs
/// out. SPEC §3.
pub struct Demod {
    cfg: DemodConfig,
    phase: Phase,
    raw_ring: VecDeque<f32>, // last AREF_HOPS raw samples, for re-estimation
    a_ref: f32,
    e_hi: f32,
    e_lo: f32,
    t: f32,
    alpha_hi: f32,
    alpha_lo: f32,
    key_down: bool,
    open: Option<Run>,
    held: Option<Run>,
    reest_done: bool,
    debounce_hops: u32,
}

impl Demod {
    /// A demodulator awaiting its first INIT_HOPS of samples to initialize.
    /// SPEC §3.1–§3.2.
    pub fn new(cfg: DemodConfig) -> Self {
        let alpha_hi = alpha_from_tau_ms(cfg.tau_hi_init_ms);
        let alpha_lo = alpha_from_tau_ms(cfg.tau_lo_ms);
        let debounce_hops = ms_to_hops(cfg.debounce_ms);
        Demod {
            cfg,
            phase: Phase::Init {
                buf: Vec::with_capacity(INIT_HOPS),
                hops_until_attempt: INIT_HOPS,
            },
            raw_ring: VecDeque::with_capacity(AREF_HOPS),
            a_ref: 1.0,
            e_hi: 0.0,
            e_lo: 0.0,
            t: 0.0,
            alpha_hi,
            alpha_lo,
            key_down: false,
            open: None,
            held: None,
            reest_done: false,
            debounce_hops,
        }
    }

    /// Whether the rails have initialized and key decisions are flowing.
    /// SPEC §3.2.
    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    /// SPEC §3.2: tau_hi = clamp(5 * dit_ms, 100, 400) ms once speed is tracked.
    pub fn set_dit_ms(&mut self, dit_ms: f32) {
        let tau =
            (5.0 * dit_ms as f64).clamp(self.cfg.tau_hi_bounds_ms.0, self.cfg.tau_hi_bounds_ms.1);
        self.alpha_hi = alpha_from_tau_ms(tau);
    }

    /// Duration of the currently-open space run, if one is open. SPEC §3.4.
    pub fn open_space_hops(&self) -> Option<u32> {
        match self.open {
            Some(r) if !r.mark => Some(r.hops),
            _ => None,
        }
    }

    /// Start timestamp of the currently-open space run, if one is open.
    /// SPEC §3.4.
    pub fn open_space_start_ts(&self) -> Option<u64> {
        match self.open {
            Some(r) if !r.mark => Some(r.start_ts),
            _ => None,
        }
    }

    /// M0 stand-in SNR from the keying rails (pinned decision 8); replaced by
    /// the SPEC §2.3 floor-based estimate at M2.
    pub fn snr_2500_db(&self) -> Option<f32> {
        if !self.running() {
            return None;
        }
        Some(20.0 * (self.e_hi / self.e_lo.max(E_LO_FLOOR)).log10() - SNR_BW_CORR_DB)
    }

    /// Feed one raw envelope sample at 375 Hz; returns any runs completed by
    /// it. SPEC §3.
    pub fn push(&mut self, a_raw: f32, sample_ts: u64) -> Vec<Run> {
        self.raw_ring.push_back(a_raw);
        if self.raw_ring.len() > AREF_HOPS {
            self.raw_ring.pop_front();
        }
        let mut out = Vec::new();
        // Borrow discipline: we can't reassign self.phase while `buf` is
        // borrowed from it, so the attempt path takes ownership of the phase.
        match &mut self.phase {
            Phase::Running => {
                self.step(a_raw, sample_ts, &mut out);
                return out;
            }
            Phase::Init {
                buf,
                hops_until_attempt,
            } => {
                buf.push((a_raw, sample_ts));
                *hops_until_attempt -= 1;
                if *hops_until_attempt > 0 {
                    return out;
                }
            }
        }
        // Attempt init on the latest INIT_HOPS window.
        let Phase::Init { mut buf, .. } = std::mem::replace(&mut self.phase, Phase::Running) else {
            unreachable!()
        };
        let window = &buf[buf.len() - INIT_HOPS..];
        // SPEC §3.1: A_ref = Q90 over the first 500 ms of the window
        // (pinned decision 10).
        let mut aref_src: Vec<f32> = window[..AREF_HOPS].iter().map(|&(a, _)| a).collect();
        aref_src.sort_by(f32::total_cmp);
        let a_ref = quantile_nearest_rank(&aref_src, 0.90).max(1e-9);
        let mut norm: Vec<f32> = window.iter().map(|&(a, _)| a / a_ref).collect();
        norm.sort_by(f32::total_cmp);
        let e_hi = quantile_nearest_rank(&norm, 0.90);
        let e_lo = quantile_nearest_rank(&norm, 0.10).max(E_LO_FLOOR);
        if e_hi / e_lo >= MIN_KEYING_RATIO {
            self.a_ref = a_ref;
            self.e_hi = e_hi;
            self.e_lo = e_lo;
            self.t = (e_hi * e_lo).sqrt();
            // Pinned decision 4: replay the init window so its elements are
            // decoded. Only the successful window replays (decision 10).
            let start = buf.len() - INIT_HOPS;
            for &(a, ts) in &buf[start..] {
                self.step(a, ts, &mut out);
            }
            // self.phase is already Running.
        } else {
            // Keep at most INIT_HOPS of history to bound memory; retry after
            // INIT_HOPS new hops (pinned decision 10).
            let excess = buf.len().saturating_sub(INIT_HOPS);
            buf.drain(..excess);
            self.phase = Phase::Init {
                buf,
                hops_until_attempt: INIT_HOPS,
            };
        }
        out
    }

    /// Takes the currently-`held` run (if any), merging in `open`'s
    /// duration first if `open` is itself a short, sub-`debounce_hops`
    /// reversal -- ordinary debounce noise riding on `held`'s trailing
    /// edge, exactly the case `step`'s live polarity-flip handling
    /// already treats as noise-not-a-new-run. `finish()` performs the
    /// same merge before emitting; matching it here keeps the reported
    /// duration accurate instead of silently short by up to
    /// `debounce_hops` (Codex review on PR #154, round 9). An `open` run
    /// that has ALREADY reached its own `debounce_hops` confirmation is
    /// untouched either way -- that one is a genuinely new, still
    /// unconfirmed run, not noise, and must never be resolved here.
    ///
    /// `held` itself has already passed its own `debounce_hops`
    /// confirmation the moment it was created (see `step`'s polarity-flip
    /// handling) -- genuinely confirmed data. It can, in principle, still
    /// be un-done by a LATER short reversal merging it back into a new
    /// `open` -- but only if more samples arrive. Once nothing more ever
    /// will (a track's true close), that can't happen, so treating `held`
    /// as final at that point is safe (round 8 -- see
    /// `TrackDecoder::finish_speed_only`, the only caller).
    pub(crate) fn take_held(&mut self) -> Option<Run> {
        match self.open {
            Some(open) if open.hops < self.debounce_hops => {
                let mut h = self.held.take()?;
                self.open = None;
                h.hops += open.hops;
                Some(h)
            }
            _ => self.held.take(),
        }
    }

    /// EOF flush: closes the open run and emits everything held. SPEC §3.4.
    pub fn finish(&mut self) -> Vec<Run> {
        let mut out = Vec::new();
        if let Some(open) = self.open.take() {
            if open.hops >= self.debounce_hops {
                if let Some(h) = self.held.take() {
                    out.push(h);
                }
                out.push(open);
            } else if let Some(mut h) = self.held.take() {
                h.hops += open.hops;
                out.push(h);
            }
        } else if let Some(h) = self.held.take() {
            out.push(h);
        }
        out
    }

    fn step(&mut self, a_raw: f32, sample_ts: u64, out: &mut Vec<Run>) {
        let a = a_raw / self.a_ref;
        // Pinned decision 9 ordering:
        // (1) rail update against previous T (SPEC §3.2: update only the rail
        //     the sample belongs to)
        if a > self.t {
            self.e_hi += self.alpha_hi * (a - self.e_hi);
        } else {
            self.e_lo += self.alpha_lo * (a - self.e_lo);
        }
        // (2) rail-collapse floor (SPEC §3.2)
        if self.e_hi < 2.0 * self.e_lo {
            self.e_hi = 2.0 * self.e_lo;
        }
        // (3) recompute T
        self.t = (self.e_hi * self.e_lo.max(E_LO_FLOOR)).sqrt();
        // SPEC §3.1: one-shot A_ref re-estimation if E_hi drifts 3x.
        if !self.reest_done
            && (self.e_hi > 3.0 || self.e_hi < 1.0 / 3.0)
            && self.raw_ring.len() == AREF_HOPS
        {
            let mut src: Vec<f32> = self.raw_ring.iter().copied().collect();
            src.sort_by(f32::total_cmp);
            let new_ref = quantile_nearest_rank(&src, 0.90).max(1e-9);
            let factor = self.a_ref / new_ref;
            self.e_hi *= factor;
            self.e_lo *= factor;
            self.t *= factor;
            self.a_ref = new_ref;
            self.reest_done = true;
        }
        // (4) key decision with hysteresis (SPEC §3.3)
        if self.key_down {
            if a < self.cfg.hyst_down * self.t {
                self.key_down = false;
            }
        } else if a > self.cfg.hyst_up * self.t {
            self.key_down = true;
        }
        // Run bookkeeping with debounce (SPEC §3.3, pinned decision 5).
        match self.open {
            None => {
                self.open = Some(Run {
                    mark: self.key_down,
                    start_ts: sample_ts,
                    hops: 1,
                });
            }
            Some(ref mut open) if open.mark == self.key_down => {
                open.hops += 1;
            }
            Some(open) => {
                // Polarity flip: close `open`.
                if open.hops < self.debounce_hops {
                    match self.held.take() {
                        Some(h) => {
                            // Merge held + short + continuing into held's polarity.
                            self.open = Some(Run {
                                mark: h.mark,
                                start_ts: h.start_ts,
                                hops: h.hops + open.hops + 1,
                            });
                        }
                        None => {
                            // Short leading run: absorbed into the new run
                            // (pinned decision 5).
                            self.open = Some(Run {
                                mark: self.key_down,
                                start_ts: open.start_ts,
                                hops: open.hops + 1,
                            });
                        }
                    }
                } else {
                    if let Some(h) = self.held.take() {
                        out.push(h);
                    }
                    self.held = Some(open);
                    self.open = Some(Run {
                        mark: self.key_down,
                        start_ts: sample_ts,
                        hops: 1,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a synthetic envelope: (level, hops) segments at 375 Hz.
    /// Returns all completed runs (including finish()).
    fn run_segments(segs: &[(f32, u32)]) -> Vec<Run> {
        let mut d = Demod::new(DemodConfig::default());
        let mut out = Vec::new();
        let mut ts = 0u64;
        for &(level, hops) in segs {
            for _ in 0..hops {
                out.extend(d.push(level, ts));
                ts += 256; // hop spacing in input samples (96 kS/s)
            }
        }
        out.extend(d.finish());
        out
    }

    #[test]
    fn alpha_constants_match_spec() {
        // SPEC §3.2: alpha_lo = 1 - e^{-2.667/500} = 0.00532
        assert!((alpha_from_tau_ms(500.0) - 0.00532).abs() < 1e-4);
        // SPEC §2.3 gives the same formula shape for tau = 40 ms: 0.0645
        assert!((alpha_from_tau_ms(40.0) - 0.0645).abs() < 5e-4);
    }

    #[test]
    fn clean_keying_yields_alternating_runs() {
        // 30-hop marks at 1.0, 30-hop spaces at 0.01, for 40 cycles (~6.4 s).
        let mut segs = Vec::new();
        for _ in 0..40 {
            segs.push((1.0f32, 30u32));
            segs.push((0.01f32, 30u32));
        }
        let runs = run_segments(&segs);
        assert!(!runs.is_empty(), "no runs emitted");
        // Alternation invariant:
        for w in runs.windows(2) {
            assert_ne!(w[0].mark, w[1].mark, "runs must alternate");
        }
        // Steady-state runs are 30 hops (allow +-2 at rail transients).
        let marks: Vec<&Run> = runs.iter().filter(|r| r.mark).collect();
        assert!(marks.len() >= 35, "got {} marks", marks.len());
        for m in &marks[2..] {
            assert!((28..=32).contains(&m.hops), "mark of {} hops", m.hops);
        }
    }

    #[test]
    fn init_replay_recovers_first_second() {
        // Pinned decision 4: elements inside the first 375-hop init window
        // must be decoded after replay. First mark starts at ts 0.
        let mut segs = Vec::new();
        for _ in 0..10 {
            segs.push((1.0f32, 30u32));
            segs.push((0.01f32, 30u32));
        }
        let runs = run_segments(&segs);
        let first = runs.iter().find(|r| r.mark).unwrap();
        assert_eq!(
            first.start_ts, 0,
            "first mark must be recovered from replay"
        );
    }

    #[test]
    fn debounce_merges_short_dropout() {
        // A 3-hop dropout (8 ms < 12 ms debounce) inside a 60-hop mark must
        // yield ONE mark of ~60 hops, not two. SPEC §3.3.
        let mut segs = vec![(1.0f32, 30u32), (0.01, 30)]; // several clean cycles first
        for _ in 0..8 {
            segs.push((1.0, 30));
            segs.push((0.01, 30));
        }
        segs.push((1.0, 28));
        segs.push((0.01, 3)); // the dropout
        segs.push((1.0, 29));
        segs.push((0.01, 40));
        let runs = run_segments(&segs);
        let big = runs.iter().filter(|r| r.mark && r.hops >= 55).count();
        assert_eq!(big, 1, "dropout must merge into one long mark: {runs:?}");
    }

    /// Codex review on PR #154, round 8: a confirmed run (past its own
    /// debounce check) sits in `held` for one further polarity flip
    /// before it reaches `on_run` live. `take_held` must surface it
    /// directly, without disturbing whatever's still accumulating in
    /// `open`.
    #[test]
    fn take_held_merges_a_short_sub_debounce_open_into_the_held_run() {
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        // Warm up clean alternating cycles first (same pattern as
        // clean_keying_yields_alternating_runs) so thresholds are
        // established.
        for _ in 0..10 {
            for _ in 0..30 {
                d.push(1.0, ts);
                ts += 256;
            }
            for _ in 0..30 {
                d.push(0.01, ts);
                ts += 256;
            }
        }
        // A fresh, long mark -- confirmed once it flips.
        for _ in 0..30 {
            d.push(1.0, ts);
            ts += 256;
        }
        // One flip: the mark above moves into `held`; a fresh 1-hop space
        // becomes `open` -- well under debounce_hops, ordinary debounce
        // noise riding on the held mark's trailing edge (round 9).
        d.push(0.01, ts);

        let held = d.take_held();
        assert!(
            matches!(held, Some(r) if r.mark && r.hops >= 26),
            "held should merge in the short open space's 1 hop on top of \
             the confirmed ~30-hop mark, got {held:?}"
        );
        assert_eq!(
            d.open_space_hops(),
            None,
            "the short open space's duration was consumed into the merge -- \
             open must be cleared, not left dangling"
        );
    }

    /// Codex review on PR #154, round 9: unlike a short sub-debounce
    /// reversal (merged in above), an `open` run that has ALREADY reached
    /// its own `debounce_hops` confirmation is a genuinely new, separate
    /// run -- not noise -- and `take_held` must never resolve it.
    #[test]
    fn take_held_leaves_an_already_confirmed_open_run_untouched() {
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        for _ in 0..10 {
            for _ in 0..30 {
                d.push(1.0, ts);
                ts += 256;
            }
            for _ in 0..30 {
                d.push(0.01, ts);
                ts += 256;
            }
        }
        for _ in 0..30 {
            d.push(1.0, ts); // fresh mark, confirmed once it flips
            ts += 256;
        }
        // Flip into a space, then keep feeding it well past debounce_hops
        // -- this space is now itself a genuinely confirmed, separate run.
        for _ in 0..30 {
            d.push(0.01, ts);
            ts += 256;
        }

        let held = d.take_held();
        assert!(
            matches!(held, Some(r) if r.mark && (25..=31).contains(&r.hops)),
            "held must report the original mark's own duration, unmerged, got {held:?}"
        );
        assert_eq!(
            d.open_space_hops(),
            Some(30),
            "an already-confirmed open run must be left untouched, not resolved"
        );
    }

    #[test]
    fn carrier_never_inits() {
        // Constant level: E_hi/E_lo < 2 -> pre-decode forever, no runs. SPEC §3.2.
        let runs = run_segments(&[(0.5, 3000)]);
        assert!(runs.is_empty(), "carrier must not produce runs: {runs:?}");
    }

    #[test]
    fn open_space_is_queryable_for_flush() {
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        for _ in 0..5 {
            for _ in 0..30 {
                d.push(1.0, ts);
                ts += 256;
            }
            for _ in 0..30 {
                d.push(0.01, ts);
                ts += 256;
            }
        }
        // Now a long open space:
        for _ in 0..100 {
            d.push(0.01, ts);
            ts += 256;
        }
        let h = d.open_space_hops().expect("open space");
        assert!(h >= 90, "open space {h} hops");
        assert!(d.open_space_start_ts().is_some());
    }

    #[test]
    fn snr_estimate_reasonable() {
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        for _ in 0..20 {
            for _ in 0..30 {
                d.push(1.0, ts);
                ts += 256;
            }
            for _ in 0..30 {
                d.push(0.02, ts); // rails ratio 50 => ~34 dB channel SNR
                ts += 256;
            }
        }
        let snr = d.snr_2500_db().unwrap();
        // 20*log10(50) - 14.3 = 34.0 - 14.3 = 19.7, with EMA settling slack
        assert!((10.0..30.0).contains(&snr), "snr {snr}");
    }
}
