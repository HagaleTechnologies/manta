//! MAN-312 (SPEC §2.7): band-wide static-crash detector.
//!
//! A static crash (lightning or another atmospheric impulse) is a few
//! milliseconds of broadband energy. The channelizer's 85.33 ms prototype
//! filter and the gate's 40 ms EMA spread it across tens of hops of every
//! channel, and each channel judging that energy on its own would start
//! hundreds of tracks. This detector judges it once per hop across the
//! channels inside the source's real RF passband (the "live channels"):
//!
//! ```text
//! lift[m] = Q25 over live channels k of (P[k,m] − R[k]) + 2.8 dB
//! ```
//!
//! where P is this hop's channel power in dB and R is the detector's own
//! per-channel dB reference, an EMA with the gate's alpha that is frozen
//! while the band is lifted. On noise the lower quartile of (P − R) sits
//! near −2.8 dB, so the lift sits near 0 dB. A crash is a lift of at least
//! `CRASH_LIFT_DB` for `CRASH_MIN_HOPS` consecutive hops, which needs at
//! least three quarters of the live channels to rise together. Below
//! `CRASH_MIN_LIVE_CHANNELS` live channels the detector is inactive.
//!
//! Pure per-hop state machine, like `floor`: what a crash does to tracks
//! and decoders is `manta-engine::track`'s job.

use crate::floor::{GATE_EMA_ALPHA, HIST_MIN_DB};

/// The lift, in dB, that counts as a crash hop: the ticket's "≥ 3 dB
/// across most of the stream". On noise the largest 3-hop lift measured
/// 0.40–1.03 dB at 512 or more live channels and 2.69 dB at 97.
pub const CRASH_LIFT_DB: f64 = 3.0;

/// Added to the lower quartile of (P − R) so that noise reads a lift of
/// about 0 dB: −Q25 of (P − R) on noise measured 2.81 dB (theory 2.90).
pub const CRASH_ONSET_Q25_OFFSET_DB: f64 = 2.8;

/// Consecutive lifted hops before a crash is declared (8 ms at 375 Hz).
/// Dense bands reached single-hop lifts of 3.27–3.38 dB but at most
/// 2.48 dB held over 3 hops.
pub const CRASH_MIN_HOPS: u64 = 3;

/// Unlifted hops that end a crash (10.7 ms): one prototype-filter
/// sidelobe period, so a crash's sidelobes do not split it in two. Every
/// measured crash was counted exactly once.
pub const CRASH_RELEASE_HOPS: u64 = 4;

/// Crash hops through which a running track's decoder input is held at its
/// pre-onset value (200 ms). Measured short crashes stayed flagged for at
/// most 30 hops; the cap bounds what a long lift can blank.
pub const CRASH_HOLD_MAX_HOPS: u64 = 75;

/// Crash hops through which track starts are suppressed (1 s). A lift
/// lasting longer is a level change, not a crash: the reference is
/// released to track it and the crash ends once it has.
pub const CRASH_MAX_HOPS: u64 = 375;

/// Fewest live channels the detector acts on. 64 live channels gave 7
/// false crashes per hour of noise; 97, 105 and 128 gave none. Rig audio
/// and captures of 6 kHz or less fall below this and filter nothing.
pub const CRASH_MIN_LIVE_CHANNELS: usize = 96;

/// One hop's verdict from `CrashDetector::update`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrashHop {
    /// This hop's lift in dB; `f64::NEG_INFINITY` when the detector was
    /// not armed or is inactive.
    pub lift_db: f64,
    /// Whether a crash is in progress after this hop.
    pub in_crash: bool,
    /// Hops since the current (or last) crash began, this hop included.
    pub run_hops: u64,
}

impl CrashHop {
    /// Start-side suppression applies: a crash is in progress and has not
    /// yet outlasted `CRASH_MAX_HOPS`.
    pub fn suppress(&self) -> bool {
        self.in_crash && self.run_hops <= CRASH_MAX_HOPS
    }

    /// The decoder hold applies: a crash is in progress and has not yet
    /// outlasted `CRASH_HOLD_MAX_HOPS`.
    pub fn hold_decoder(&self) -> bool {
        self.in_crash && self.run_hops <= CRASH_HOLD_MAX_HOPS
    }
}

/// The band-wide crash state machine. Call `update` once per hop with
/// that hop's per-channel dB power (the same vector `FloorBank::update`
/// takes).
#[derive(Debug, Clone)]
pub struct CrashDetector {
    reference_db: Vec<f64>,
    ref_init: bool,
    live: Vec<usize>,
    /// Per-hop scratch for (P − R) over the live set, reused so `update`
    /// does not allocate.
    excess: Vec<f64>,
    above_run: u64,
    in_crash: bool,
    below_hops: u64,
    run_hops: u64,
    count: u64,
}

impl CrashDetector {
    /// A detector for `n_channels` channels, every one of them live.
    pub fn new(n_channels: usize) -> Self {
        CrashDetector {
            reference_db: vec![HIST_MIN_DB; n_channels],
            ref_init: false,
            live: (0..n_channels).collect(),
            excess: Vec::with_capacity(n_channels),
            above_run: 0,
            in_crash: false,
            below_hops: 0,
            run_hops: 0,
            count: 0,
        }
    }

    /// The channels inside the source's real RF passband. Each index must
    /// be below `n_channels`.
    pub fn set_live(&mut self, live: Vec<usize>) {
        let n = self.reference_db.len();
        assert!(
            live.iter().all(|&k| k < n),
            "CrashDetector::set_live: a live channel index is not below n_channels {n}"
        );
        self.excess = Vec::with_capacity(live.len());
        self.live = live;
    }

    /// How many channels the lift is judged over.
    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Whether the passband holds enough channels for the detector to act.
    pub fn is_active(&self) -> bool {
        self.live.len() >= CRASH_MIN_LIVE_CHANNELS
    }

    /// Crashes entered so far.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// One hop. `armed` is false while the pipeline warms up: those hops
    /// only warm the reference and never detect.
    pub fn update(&mut self, power_db: &[f64], armed: bool) -> CrashHop {
        assert_eq!(
            power_db.len(),
            self.reference_db.len(),
            "CrashDetector::update: power_db length {} does not match n_channels {}",
            power_db.len(),
            self.reference_db.len()
        );
        if !self.ref_init {
            self.reference_db.copy_from_slice(power_db);
            self.ref_init = true;
        }

        let lift_db = if armed && self.is_active() {
            self.excess.clear();
            self.excess.extend(
                self.live
                    .iter()
                    .map(|&k| power_db[k] - self.reference_db[k]),
            );
            let q = self.excess.len() / 4;
            *self.excess.select_nth_unstable_by(q, f64::total_cmp).1 + CRASH_ONSET_Q25_OFFSET_DB
        } else {
            f64::NEG_INFINITY
        };

        let above = lift_db >= CRASH_LIFT_DB;
        self.above_run = if above { self.above_run + 1 } else { 0 };

        if above && (self.in_crash || self.above_run >= CRASH_MIN_HOPS) {
            self.below_hops = 0;
            if !self.in_crash {
                self.in_crash = true;
                self.count += 1;
                self.run_hops = 0;
            }
        } else if self.in_crash {
            self.below_hops += 1;
            if self.below_hops >= CRASH_RELEASE_HOPS {
                self.in_crash = false;
            }
        }
        if self.in_crash {
            self.run_hops += 1;
        }

        // Frozen from the first lifted hop, so the reference stays the
        // pre-crash band; released past CRASH_MAX_HOPS so a level change
        // is tracked and ends the crash.
        let frozen = (above || self.in_crash) && !(self.in_crash && self.run_hops > CRASH_MAX_HOPS);
        if !frozen {
            for (r, &p) in self.reference_db.iter_mut().zip(power_db) {
                if *r < HIST_MIN_DB {
                    // Out of digital silence: snap rather than climb from
                    // -200 dB at the EMA's pace.
                    *r = p;
                } else {
                    *r += GATE_EMA_ALPHA * (p - *r);
                }
            }
        }

        CrashHop {
            lift_db,
            in_crash: self.in_crash,
            run_hops: self.run_hops,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 128;
    const BASE_DB: f64 = -90.0;

    fn flat(db: f64) -> Vec<f64> {
        vec![db; N]
    }

    /// A detector whose reference sits exactly on a flat -90 dB band, so a
    /// flat hop reads a lift of exactly `CRASH_ONSET_Q25_OFFSET_DB`.
    fn settled() -> CrashDetector {
        let mut det = CrashDetector::new(N);
        for _ in 0..10 {
            det.update(&flat(BASE_DB), true);
        }
        assert_eq!(det.count(), 0);
        det
    }

    fn run(det: &mut CrashDetector, db: f64, hops: usize) -> CrashHop {
        let p = flat(db);
        let mut last = det.update(&p, true);
        for _ in 1..hops {
            last = det.update(&p, true);
        }
        last
    }

    #[test]
    fn crash_needs_three_consecutive_lifted_hops() {
        let mut det = settled();
        let h = run(&mut det, BASE_DB + 20.0, 2);
        assert!(!h.in_crash);
        run(&mut det, BASE_DB, 10);
        assert_eq!(det.count(), 0, "two lifted hops must not make a crash");

        let h = run(&mut det, BASE_DB + 20.0, 2);
        assert!(!h.in_crash);
        let h = run(&mut det, BASE_DB + 20.0, 1);
        assert!(h.in_crash && h.suppress() && h.hold_decoder());
        assert_eq!(h.run_hops, 1);
        assert_eq!(det.count(), 1);
    }

    #[test]
    fn crash_is_judged_on_the_lower_quartile() {
        // 24% of 128 channels is 30.72: 30 lifted channels leave the lower
        // quartile on the unlifted band.
        let mut det = settled();
        let mut p = flat(BASE_DB);
        for v in p.iter_mut().take(30) {
            *v += 30.0;
        }
        for _ in 0..10 {
            let h = det.update(&p, true);
            assert_eq!(h.lift_db, CRASH_ONSET_Q25_OFFSET_DB);
        }
        assert_eq!(det.count(), 0, "a 24% lift is not band-wide");

        // 80% (102 channels) lifts the lower quartile.
        let mut det = settled();
        let mut p = flat(BASE_DB);
        for v in p.iter_mut().take(102) {
            *v += 30.0;
        }
        for _ in 0..10 {
            det.update(&p, true);
        }
        assert_eq!(det.count(), 1, "an 80% lift is a crash");
    }

    #[test]
    fn release_merges_gaps_shorter_than_four_hops() {
        let mut det = settled();
        run(&mut det, BASE_DB + 20.0, 3);
        let h = run(&mut det, BASE_DB, 3);
        assert!(h.in_crash, "a 3-hop gap must not end the crash");
        run(&mut det, BASE_DB + 20.0, 3);
        run(&mut det, BASE_DB, 10);
        assert_eq!(det.count(), 1);

        let mut det = settled();
        run(&mut det, BASE_DB + 20.0, 3);
        let h = run(&mut det, BASE_DB, 4);
        assert!(!h.in_crash, "a 4-hop gap ends the crash");
        run(&mut det, BASE_DB + 20.0, 3);
        run(&mut det, BASE_DB, 10);
        assert_eq!(det.count(), 2);
    }

    #[test]
    fn caps() {
        let mut det = settled();
        let lifted = flat(BASE_DB + 10.0);
        let (mut saw_hold_end, mut saw_suppress_end) = (false, false);
        let mut cap_hop = None;
        let mut release_hop = None;
        for hop in 0..1000usize {
            let h = det.update(&lifted, true);
            assert!(det.count() <= 1, "a sustained lift is one crash");
            if h.in_crash {
                assert_eq!(h.hold_decoder(), h.run_hops <= CRASH_HOLD_MAX_HOPS);
                assert_eq!(h.suppress(), h.run_hops <= CRASH_MAX_HOPS);
                saw_hold_end |= h.run_hops == CRASH_HOLD_MAX_HOPS + 1 && !h.hold_decoder();
                saw_suppress_end |= h.run_hops == CRASH_MAX_HOPS + 1 && !h.suppress();
                if h.run_hops == CRASH_MAX_HOPS {
                    assert!(h.suppress() && !h.hold_decoder());
                    cap_hop = Some(hop);
                }
            } else if cap_hop.is_some() && release_hop.is_none() {
                assert!(!h.suppress() && !h.hold_decoder());
                release_hop = Some(hop);
            }
        }
        assert!(saw_hold_end && saw_suppress_end);
        assert_eq!(det.count(), 1);
        let (cap, release) = (cap_hop.unwrap(), release_hop.expect("the crash must end"));
        assert!(
            release - cap <= CRASH_RELEASE_HOPS as usize + 64,
            "released {} hops after the cap",
            release - cap
        );
    }

    #[test]
    fn inactive_below_96_live_channels() {
        let mut det = settled();
        det.set_live((0..CRASH_MIN_LIVE_CHANNELS - 1).collect());
        assert_eq!(det.live_len(), 95);
        assert!(!det.is_active());
        let h = run(&mut det, BASE_DB + 40.0, 10);
        assert_eq!(h.lift_db, f64::NEG_INFINITY);
        assert_eq!(det.count(), 0);

        let mut det = settled();
        det.set_live((0..CRASH_MIN_LIVE_CHANNELS).collect());
        assert!(det.is_active());
        run(&mut det, BASE_DB + 40.0, 10);
        assert_eq!(det.count(), 1, "96 live channels is enough");
    }

    #[test]
    fn silence_reference_snaps() {
        // Unarmed: the first hop after digital silence snaps the reference
        // instead of climbing from -200 dB at the EMA's pace.
        let mut det = CrashDetector::new(N);
        det.update(&flat(-200.0), false);
        det.update(&flat(BASE_DB), false);
        assert!(det.reference_db.iter().all(|&r| r == BASE_DB));

        // Armed: signal after silence is one crash whose reference stays
        // frozen at -200 dB until the cap, then snaps and releases.
        let mut det = CrashDetector::new(N);
        det.update(&flat(-200.0), true);
        let mut snapped_at = None;
        let mut released_at = None;
        for hop in 0..CRASH_MAX_HOPS as usize + 20 {
            let h = det.update(&flat(BASE_DB), true);
            match snapped_at {
                None if h.run_hops <= CRASH_MAX_HOPS => {
                    assert!(det.reference_db.iter().all(|&r| r == -200.0));
                }
                None => {
                    assert!(h.in_crash);
                    assert_eq!(h.run_hops, CRASH_MAX_HOPS + 1);
                    assert!(det.reference_db.iter().all(|&r| r == BASE_DB));
                    snapped_at = Some(hop);
                }
                Some(_) if !h.in_crash && released_at.is_none() => released_at = Some(hop),
                Some(_) => {}
            }
        }
        let (snap, release) = (snapped_at.unwrap(), released_at.unwrap());
        assert_eq!(release - snap, CRASH_RELEASE_HOPS as usize);
        assert_eq!(det.count(), 1);
    }

    #[test]
    fn unarmed_hops_only_warm_the_reference() {
        let mut det = CrashDetector::new(N);
        det.update(&flat(BASE_DB), false);
        let h = det.update(&flat(BASE_DB + 40.0), false);
        assert_eq!(h.lift_db, f64::NEG_INFINITY);
        assert!(!h.in_crash);
        let want = BASE_DB + GATE_EMA_ALPHA * 40.0;
        assert!(det.reference_db.iter().all(|&r| r == want));

        for _ in 0..500 {
            let h = det.update(&flat(BASE_DB + 40.0), false);
            assert!(!h.in_crash && !h.suppress() && !h.hold_decoder());
        }
        assert_eq!(det.count(), 0);
        // Warmed onto the new level, the first armed hops see no lift.
        let h = run(&mut det, BASE_DB + 40.0, 10);
        assert!(h.lift_db < CRASH_LIFT_DB);
        assert_eq!(det.count(), 0);
    }
}
