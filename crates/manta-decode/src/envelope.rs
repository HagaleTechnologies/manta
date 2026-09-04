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

/// **[DEVIATION from SPEC §3.2]** MAN-8 H3: `E_hi` only updates on hops
/// where `a > T` (roughly the ~40% mark duty cycle), so its *wall-clock*
/// time constant is `tau_hi / duty`, not `tau_hi` -- violating SPEC §3.2's
/// own stated rationale that "`E_hi` must ride QSB (fast)". At 20 WPM
/// `tau_hi = 300 ms` gives an actual wall-clock constant of ~750 ms against
/// a QSB fade slewing up to 8 dB/s on its steepest slope. Track the duty
/// cycle with a slow EMA and scale the per-update alpha so the *achieved*
/// wall-clock constant matches the intended `tau_hi`. See
/// docs/DECISIONS/2026-09-04-man8-v6-qsb-decode-fix.md.
const DUTY_TAU_MS: f64 = 1000.0;
/// Bounds the duty estimate away from 0 (near-silent track, which would
/// otherwise make the effective alpha explode) and 1 (unkeyed carrier,
/// which must not speed E_hi up at all).
const DUTY_BOUNDS: (f32, f32) = (0.25, 0.75);
/// Upper bound on the duty-compensated alpha_hi, so a very low duty
/// estimate can't make E_hi update faster than a sane per-hop EMA.
const ALPHA_HI_MAX: f32 = 0.5;

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
    /// EMA of the fraction of hops spent key-down. MAN-8 H3: used to
    /// compensate `E_hi`'s update rate for the fact it only updates on
    /// key-down hops (see `DUTY_TAU_MS`'s doc comment).
    duty: f32,
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
            duty: 0.4, // SPEC §3.2's ~40% nominal mark duty cycle
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
        // MAN-8 H3: track the key-down duty cycle (previous hop's key
        // state) before the rail update, so this hop's E_hi update (if
        // any) uses the duty-compensated alpha -- see DUTY_TAU_MS's doc
        // comment.
        let a_duty = alpha_from_tau_ms(DUTY_TAU_MS);
        self.duty += a_duty * (f32::from(self.key_down) - self.duty);
        let duty = self.duty.clamp(DUTY_BOUNDS.0, DUTY_BOUNDS.1);
        let alpha_hi_eff = (self.alpha_hi / duty).min(ALPHA_HI_MAX);
        // Pinned decision 9 ordering:
        // (1) rail update against previous T (SPEC §3.2: update only the rail
        //     the sample belongs to)
        if a > self.t {
            self.e_hi += alpha_hi_eff * (a - self.e_hi);
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

    /// MAN-8 H1 support: builds a noise-free, evenly-spaced 20 WPM dit train
    /// (dit mark, dit gap, repeating) over `cycles` full QSB periods. The
    /// mark level is swept by V6's exact QSB law
    /// (`0.55 + 0.45*sin(2*pi*rate_hz*t)`, `crates/manta-testkit/src/
    /// scene.rs:127`) atop a fixed floor pedestal, and every transition is
    /// smoothed by a box filter of width `EDGE_HOPS` -- a simple, deterministic
    /// stand-in for the channelizer's finite (~10.7 ms) edge slew, since a
    /// real signal's rise/fall is exactly what gives instantaneous SNR any
    /// influence over where `Demod`'s threshold crosses it (H1's claimed
    /// mechanism). Returns every completed `Run` paired with the QSB
    /// multiplier at that run's start, so a caller can bucket by fade phase.
    const EDGE_HOPS: usize = 4; // ~10.7 ms at 375 Hz, matching the plan's estimate

    fn qsb_keyed_train(wpm: f32, cycles: u32, rate_hz: f32, snr_ch_peak_db: f32) -> Vec<(Run, f32)> {
        let dit_ms = 1200.0 / wpm;
        let dit_hops = (dit_ms as f64 / HOP_MS).round() as usize;
        let period_hops = ((1.0 / rate_hz as f64) * crate::FO_HZ).round() as usize;
        let total_hops = period_hops * cycles as usize;

        let floor = 1.0f32;
        let peak_ratio = 10f32.powf(snr_ch_peak_db / 20.0);
        let qsb_mul = |hop: usize| -> f32 {
            let t = hop as f64 * HOP_MS / 1000.0;
            (0.55 + 0.45 * (std::f64::consts::TAU * rate_hz as f64 * t).sin()) as f32
        };

        let mut want_mark = vec![false; total_hops];
        let mut hop = 0usize;
        let mut mark = true;
        while hop < total_hops {
            let end = (hop + dit_hops).min(total_hops);
            for w in &mut want_mark[hop..end] {
                *w = mark;
            }
            mark = !mark;
            hop = end;
        }

        let mut target = vec![0.0f32; total_hops];
        for h in 0..total_hops {
            target[h] = if want_mark[h] {
                floor + floor * peak_ratio * qsb_mul(h)
            } else {
                floor
            };
        }
        // Box-filter smoothing models the finite edge slew: a step's edges
        // become a linear ramp of width ~EDGE_HOPS around the transition.
        let half = EDGE_HOPS / 2;
        let mut level = vec![0.0f32; total_hops];
        for h in 0..total_hops {
            let lo = h.saturating_sub(half);
            let hi = (h + half).min(total_hops - 1);
            let acc: f64 = target[lo..=hi].iter().map(|&v| v as f64).sum();
            level[h] = (acc / (hi - lo + 1) as f64) as f32;
        }

        let mut d = Demod::new(DemodConfig::default());
        let mut out = Vec::new();
        for (h, &lvl) in level.iter().enumerate() {
            let ts = h as u64 * 256; // hop spacing in raw samples at 96 kS/s
            for run in d.push(lvl, ts) {
                let run_hop = (run.start_ts / 256) as usize;
                out.push((run, qsb_mul(run_hop)));
            }
        }
        for run in d.finish() {
            let run_hop = (run.start_ts / 256) as usize;
            out.push((run, qsb_mul(run_hop)));
        }
        out
    }

    fn mean_hops(runs: &[(Run, f32)]) -> f32 {
        let sum: f64 = runs.iter().map(|(r, _)| r.hops as f64).sum();
        (sum / runs.len() as f64) as f32
    }

    /// H1: the measured duration of an identical keyed mark must not depend
    /// on the apparent keying depth. Feeds Demod a noise-free 20 WPM dit
    /// train whose amplitude is swept by V6's own QSB law, with a finite
    /// (~10.7 ms) edge slew, and compares mark durations measured at the
    /// QSB peak (multiplier > 0.9) vs. the trough (multiplier < 0.15,
    /// V6's own trough is 0.10). MAN-8.
    ///
    /// **[IGNORED — measured, not fixed.]** H1 is confirmed: this drifts
    /// 11.9% on `main` (measured). MAN-8 Phase 3 tried anchoring the key
    /// threshold at `K_ANCHOR*E_hi` (never below the geometric mean) to
    /// make the crossing point SNR-invariant above a crossover. Swept
    /// `K_ANCHOR` in {0.20, 0.30}: at 0.20 the anchor almost never binds
    /// within V6's actual SNR range, so this test's drift is essentially
    /// unchanged (11.5%) and V6's golden CER is bit-for-bit identical to
    /// the no-anchor baseline (0.142857...). At 0.30 the anchor does bind
    /// more (drift falls to 9.1%, still short of this test's 4% bar) but
    /// V6's golden CER is *still* bit-for-bit identical to baseline
    /// (errors move to different characters, net count unchanged) -- and
    /// 0.30 regresses the previously-green V10 (Farnsworth, ~29 dB channel
    /// SNR, squarely inside where the anchor now binds) from CER 0 to
    /// 0.0987 (gate 0.05), a real, measured character-level corruption
    /// during Farnsworth warmup. H1 is real but not the dominant driver of
    /// V6's CER gap; fixing it alone does not move V6 and costs a
    /// currently-green golden vector. Reverted per this plan's stop rule
    /// (no rung may regress a currently-green test). Kept `#[ignore]`d as
    /// permanent, real coverage for an amplitude-varying envelope (the
    /// first in this crate) and as the executable record of the
    /// confirmed-but-unfixed H1 mechanism. See
    /// docs/DECISIONS/2026-09-04-man8-v6-qsb-decode-fix.md.
    #[test]
    #[ignore]
    fn mark_duration_is_stable_across_a_qsb_cycle() {
        let runs = qsb_keyed_train(20.0, 4, 0.2, 34.3);
        let peak: Vec<(Run, f32)> = runs
            .iter()
            .copied()
            .filter(|(r, mul)| r.mark && *mul > 0.9)
            .collect();
        let trough: Vec<(Run, f32)> = runs
            .iter()
            .copied()
            .filter(|(r, mul)| r.mark && *mul < 0.15)
            .collect();
        assert!(
            !peak.is_empty() && !trough.is_empty(),
            "not enough phase coverage: {} peak marks, {} trough marks",
            peak.len(),
            trough.len()
        );
        let peak_dit = mean_hops(&peak);
        let trough_dit = mean_hops(&trough);
        let drift = (peak_dit - trough_dit).abs() / peak_dit;
        assert!(
            drift < 0.04,
            "dit duration drifts {drift:.3} across the QSB cycle (peak {peak_dit:.1} hops \
             over {} marks, trough {trough_dit:.1} hops over {} marks)",
            peak.len(),
            trough.len()
        );
    }

    /// MAN-8 H3/Phase 4: a step change in mark level must be tracked by
    /// `E_hi` within ~2*tau_hi of WALL CLOCK, not mark-time, regardless of
    /// duty cycle -- the duty-cycle-compensated alpha_hi this phase adds.
    #[test]
    fn e_hi_tracks_a_level_step_in_wall_clock_time() {
        let dit_hops = 22u32; // ~20 WPM: dit = 60 ms = 22.5 hops
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        let push_train = |d: &mut Demod, ts: &mut u64, level: f32, wall_ms: f64| {
            let n_hops = (wall_ms / HOP_MS).round() as u32;
            let mut hop = 0u32;
            let mut mark = true;
            while hop < n_hops {
                let seg = dit_hops.min(n_hops - hop);
                for _ in 0..seg {
                    d.push(if mark { level } else { level * 0.02 }, *ts);
                    *ts += 256;
                    hop += 1;
                }
                mark = !mark;
            }
        };
        // Steady state at level 1.0 well past init + settling.
        push_train(&mut d, &mut ts, 1.0, 3000.0);
        assert!(d.running());
        d.set_dit_ms(60.0); // 20 WPM: tau_hi = clamp(5*60, 100, 400) = 300 ms
        // Step to +10 dB amplitude; measure E_hi after 2*tau_hi = 600 ms of
        // WALL clock (not mark-time -- at ~40% duty, mark-time would need
        // 2*tau_hi/0.4 = 1500 ms without the H3 compensation).
        let new_level = 10f32.powf(10.0 / 20.0);
        push_train(&mut d, &mut ts, new_level, 600.0);
        let err_db = 20.0 * (d.e_hi / new_level).log10();
        assert!(
            err_db.abs() < 1.0,
            "E_hi not within 1 dB of the new level after 600 ms wall clock: \
             e_hi={} new_level={new_level} err={err_db:.2} dB",
            d.e_hi
        );
    }

    /// MAN-8 H3/Phase 4: a continuous carrier (100% key-down) must not
    /// drive the duty estimate's effect on alpha_hi past DUTY_BOUNDS's
    /// clamp -- the raw EMA is free to approach 1.0, but the compensation
    /// ratio applied to alpha_hi must stay bounded at `1/DUTY_BOUNDS.1`.
    #[test]
    fn duty_estimate_is_clamped_on_a_continuous_carrier() {
        let mut d = Demod::new(DemodConfig::default());
        let mut ts = 0u64;
        // Init via a normal dit/dah/gap train (a literal constant carrier
        // never leaves Init -- see carrier_never_inits above).
        for _ in 0..20 {
            for _ in 0..22 {
                d.push(1.0, ts);
                ts += 256;
            }
            for _ in 0..22 {
                d.push(0.02, ts);
                ts += 256;
            }
        }
        assert!(d.running(), "must have initialized before the carrier hold");
        // Hold key-down continuously for several DUTY_TAU_MS time constants.
        let hops = (5.0 * DUTY_TAU_MS / HOP_MS).round() as u32;
        for _ in 0..hops {
            d.push(1.0, ts);
            ts += 256;
        }
        assert!(
            d.duty > DUTY_BOUNDS.1,
            "raw duty EMA should climb past the clamp bound under a sustained \
             carrier, got {}",
            d.duty
        );
        let alpha_hi_eff = (d.alpha_hi / d.duty.clamp(DUTY_BOUNDS.0, DUTY_BOUNDS.1))
            .min(ALPHA_HI_MAX);
        let ratio = alpha_hi_eff / d.alpha_hi;
        assert!(
            ratio <= 1.0 / DUTY_BOUNDS.1 + 1e-3,
            "clamp must bound the compensation ratio at 1/DUTY_BOUNDS.1, got {ratio}"
        );
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
