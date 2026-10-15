//! Evidence front end: smoothed centered-max mark level, keying gate,
//! half-amplitude-centered per-hop log-likelihood ratio, anchors, prefix
//! sums. SPEC v2 §1.

use crate::HOP_MS;
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct EvidenceConfig {
    pub sigma_u: f32,
    pub llr_clip: f32,
    pub hold_dits: f32,
    pub fallback_hops: u32,
    pub tau_a_ms: f64,
    pub u_init_hops: f32,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        EvidenceConfig {
            sigma_u: 0.30,
            llr_clip: 10.0,
            hold_dits: 4.0,
            fallback_hops: 8,
            tau_a_ms: 8.0,
            u_init_hops: 15.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HopEvidence {
    pub hop: u64,
    pub sample_ts: u64,
    pub llr: f32,
    pub present: bool,
    pub anchor: bool,
    pub prefix: f64,
    pub amp: f32,
    pub mark_level: f32,
    pub noise_level: f32,
}

pub struct Evidence {
    cfg: EvidenceConfig,
    alpha_a: f32,
    a_s: Option<f32>,
    h: usize,
    /// Delay line of (raw amp, smoothed amp, noise amp, sample_ts); the
    /// element at index `h` from the back is the one being emitted.
    line: VecDeque<(f32, f32, f32, u64)>,
    hop_in: u64,
    hop_out: u64,
    prefix: f64,
    last_present: bool,
    /// One computed hop retained until its successor's actual LLR sign is
    /// known (MAN-190). The pending hop's fields are frozen at computation
    /// time; only its `anchor` flag is finalized later, from the two hops'
    /// real signs. A caller's `set_u_ref` between releases changes `h` for
    /// the next *computed* center only and can never rewrite this value.
    pending: Option<HopEvidence>,
    /// Global (push-order) index of the next center to compute.
    /// Independent of `self.h`, which may change between pushes via
    /// `set_u_ref` -- this is what makes speed changes safe: computation
    /// order and count depend only on this counter advancing by exactly 1
    /// per computed hop, never on `h`. Release order additionally lags
    /// computation by the one-hop pending slot.
    next_center_g: u64,
    /// SPEC v2 §1.2's `M` for the actual centered window (MAN-193).
    max_cache: CenteredMax,
    /// Test-only count of smoothed-amplitude reads made to find `M`.
    #[cfg(test)]
    max_reads: std::cell::Cell<u64>,
    /// Test-only slow reference: find both maxima by the original
    /// zero-seeded fold over the window instead of `max_cache`.
    #[cfg(test)]
    reference_scan: bool,
}

/// Generous fixed retention cap for the delay line, independent of `h` --
/// large enough for any realistic `hold_dits * u_max` (SPEC v2 §7: u_max
/// default 56 hops, hold_dits default 4 -> h ~= 224 at the extreme; retaining
/// several multiples of that bounds memory without ever needing to trim in
/// a way that's coupled to a changing `h`).
///
/// Public (Codex review, PR #161 round 6) so a config loader can validate
/// a user-supplied `hold_dits` against it BEFORE constructing `Evidence` --
/// a config with a large but individually-plausible `hold_dits` (e.g. 300)
/// pushes `h` past this cap, tripping the `debug_assert!`s below in a debug
/// build or, in release, silently capping the delay line and discarding
/// centers no downstream code is ever told about.
pub const MAX_RETAIN: usize = 4096;

impl Evidence {
    pub fn new(cfg: EvidenceConfig) -> Self {
        let alpha_a = (1.0 - (-HOP_MS / cfg.tau_a_ms).exp()) as f32;
        let h = (cfg.hold_dits * cfg.u_init_hops).round().max(1.0) as usize;
        debug_assert!(
            h < MAX_RETAIN,
            "h ({}) must stay well under MAX_RETAIN ({}) or Evidence silently drops un-emitted centers",
            h,
            MAX_RETAIN
        );
        Evidence {
            cfg,
            alpha_a,
            a_s: None,
            h,
            line: VecDeque::new(),
            hop_in: 0,
            hop_out: 0,
            prefix: 0.0,
            last_present: false,
            pending: None,
            next_center_g: 0,
            max_cache: CenteredMax::default(),
            #[cfg(test)]
            max_reads: Default::default(),
            #[cfg(test)]
            reference_scan: false,
        }
    }

    /// Test-only: an `Evidence` that finds `M` by the original full-window
    /// scan, the oracle the cached path is compared against.
    #[cfg(test)]
    fn reference(cfg: EvidenceConfig) -> Self {
        Evidence {
            reference_scan: true,
            ..Evidence::new(cfg)
        }
    }

    /// SPEC v2 §1.2: h = round(hold_dits * u_ref). Sets the hold window
    /// for the next evidence hop *computed*. One computed hop may be
    /// pending until its successor confirms its anchor. Updating the
    /// window never recomputes that pending hop. During incremental HSMM
    /// decoding, feedback after returned hop t first affects computation
    /// of hop t+2.
    pub fn set_u_ref(&mut self, u_hops: f32) {
        self.h = (self.cfg.hold_dits * u_hops).round().max(1.0) as usize;
        debug_assert!(
            self.h < MAX_RETAIN,
            "h ({}) must stay well under MAX_RETAIN ({}) or Evidence silently drops un-emitted centers",
            self.h,
            MAX_RETAIN
        );
    }

    /// Record one new input sample into the delay line. Emits nothing
    /// itself -- call `drain_one` afterward (in a loop) to read off every
    /// center this sample made eligible. See `drain_one`'s doc comment
    /// for why callers that need per-hop feedback must use this split
    /// instead of `push`.
    pub(crate) fn record(&mut self, amp: f32, noise_amp: f32, sample_ts: u64) {
        let a_s = match self.a_s {
            None => amp,
            Some(p) => p + self.alpha_a * (amp - p),
        };
        self.a_s = Some(a_s);
        self.line.push_back((amp, a_s, noise_amp, sample_ts));
        self.hop_in += 1;
        if self.line.len() > MAX_RETAIN {
            self.line.pop_front();
        }
    }

    /// Drain exactly one finalized center, or `None` if none is finalized
    /// yet. Returning `None` does not mean nothing was computed: one
    /// already-computed hop may be retained in the pending slot until its
    /// successor is computed (a window growth can stall that computation
    /// until more input arrives).
    ///
    /// Codex review, PR #161 round 11 (amended by MAN-190): round 9 fixed
    /// `push` under-draining a shrunk hold window's backlog by looping
    /// internally and returning the WHOLE newly-eligible batch in one
    /// `Vec`. But every hop in a single batch was centered using the SAME
    /// `self.h` -- whatever it was when the call started -- since nothing
    /// inside one `push` call can update `h` mid-loop (`set_u_ref` is only
    /// ever called by the CALLER, after the whole batch already came
    /// back). This method plus `record` let a caller (`push_hop_hsmm`)
    /// apply `set_u_ref` feedback BETWEEN every single finalized hop.
    /// MAN-190 moved the anchor decision from forward prediction to
    /// confirmation by the successor's actual sign, so feedback after
    /// returned hop t first affects the computation of hop t+2 (hop t+1
    /// is already computed and pending when t is returned).
    pub fn drain_one(&mut self) -> Option<HopEvidence> {
        if self.pending.is_none() {
            // Prime the pending slot with the next eligible center, or
            // report nothing available.
            self.pending = Some(self.compute_eligible()?);
        }
        // Finalize the pending hop against its actual successor once that
        // successor is eligible; otherwise retain the pending hop.
        self.compute_eligible()
            .map(|successor| self.finalize_pending(successor))
    }

    /// Record one input sample and drain every center it makes eligible
    /// in one call. Correct for callers with no per-hop feedback to apply
    /// between drained hops (EdgeLegacy, which never calls `set_u_ref`;
    /// tests) -- see `drain_one`'s doc comment for why a caller that DOES
    /// have such feedback (`push_hop_hsmm`) must use `record`+`drain_one`
    /// directly instead of this convenience wrapper.
    pub fn push(&mut self, amp: f32, noise_amp: f32, sample_ts: u64) -> Vec<HopEvidence> {
        self.record(amp, noise_amp, sample_ts);
        let mut out = Vec::new();
        while let Some(ev) = self.drain_one() {
            out.push(ev);
        }
        out
    }

    /// Drain and emit exactly the next finalized hop, or `None` once the
    /// delay line is exhausted AND the pending slot is empty.
    ///
    /// Codex review, PR #161 round 4 (amended by MAN-190): unlike `flush()`
    /// (which eagerly materializes the WHOLE remaining tail in one call),
    /// this lets a caller apply the live path's per-hop feedback --
    /// `HsmmDecoder`'s `best_u()` -> `Evidence::set_u_ref()` loop that
    /// `push_hop_hsmm` runs after every single hop -- between successive
    /// flushed hops too. `decoder::finish()`'s old `Engine::Hsmm` branch
    /// called the eager `flush()` up front and only afterward looped over
    /// the already-computed results, so that feedback never ran during
    /// the tail: every flushed hop was centered/gated using whichever `h`
    /// (hold-window width) was in effect from the last LIVE hop, even if
    /// the true speed changes right at end-of-recording -- often still
    /// the initial ~15-hop seed unit for a short input that never got far
    /// enough into `push_hop_hsmm` to adapt. That stale window could
    /// alter or lose the final characters.
    ///
    /// MAN-190: each call computes at most one further center with the
    /// currently effective `h` (forward reach clipped to the last real
    /// input) and finalizes the pending hop against it. When no uncomputed
    /// center remains, the final pending hop is released exactly once,
    /// keeping only its own periodic/gate base anchor -- no synthetic
    /// successor, no zero padding, no shortened backward window.
    pub fn flush_one(&mut self) -> Option<HopEvidence> {
        if self.pending.is_none() {
            // Prime the pending slot with the next uncomputed center, or
            // report the stream exhausted.
            self.pending = Some(self.compute_any()?);
        }
        match self.compute_any() {
            Some(successor) => Some(self.finalize_pending(successor)),
            None => self.pending.take(), // terminal release, base anchor only
        }
    }

    pub fn flush(&mut self) -> Vec<HopEvidence> {
        // SDD execution 2026-09-09, Task 8a: `next_center_g` is a global,
        // `h`-independent counter, so draining here needs no special-casing
        // of `self.h` at all (unlike the old center-derivation this
        // replaced) -- just keep emitting the next global center until the
        // buffer is exhausted.
        let mut out = Vec::new();
        while let Some(ev) = self.flush_one() {
            out.push(ev);
        }
        out
    }

    /// Global (push-order) index of `line[0]`.
    fn front_global(&self) -> u64 {
        self.hop_in - self.line.len() as u64
    }

    /// Local delay-line index of the next center to compute, after the
    /// fell-behind guard.
    fn next_local(&mut self) -> usize {
        let front_global = self.front_global();
        if self.next_center_g < front_global {
            // Fell behind the retained window (should not happen with a
            // generous MAX_RETAIN in practice); jump to the oldest
            // available sample rather than panic on the subtraction below.
            self.next_center_g = front_global;
        }
        (self.next_center_g - front_global) as usize
    }

    /// Compute the next center if its full forward window is resident
    /// (the live-path eligibility rule). `checked_add` guards against a
    /// pathological `h` (e.g. from an unvalidated `set_u_ref` input)
    /// overflowing `usize`; treat an overflow the same as "not enough
    /// lookahead yet" rather than panicking (debug) or wrapping into a
    /// spurious true / OOB index (release). The debug_assert!s in
    /// `new()`/`set_u_ref()` are the primary guard -- this is the
    /// release-build backstop.
    fn compute_eligible(&mut self) -> Option<HopEvidence> {
        let local = self.next_local();
        match local.checked_add(self.h) {
            Some(reach) if reach < self.line.len() => Some(self.compute(local)),
            _ => None, // not enough forward lookahead yet (or overflow)
        }
    }

    /// Compute the next center if one exists at all, clipping its forward
    /// reach to the last real input (the EOF rule; backward reach is never
    /// clipped -- those samples are still resident).
    fn compute_any(&mut self) -> Option<HopEvidence> {
        let local = self.next_local();
        (local < self.line.len()).then(|| self.compute(local))
    }

    /// `M` over the retained local interval `[lo, hi]`, from `max_cache`
    /// (MAN-193: O(1) amortized while the window's bounds move forward).
    /// MAN-190 computes every center exactly once, in push order, so the
    /// bounds only retreat when `set_u_ref` changes `h`.
    fn centered_max(&mut self, lo: usize, hi: usize) -> f32 {
        #[cfg(test)]
        if self.reference_scan {
            return self.scan_max(lo, hi);
        }
        let front = self.front_global();
        let line = &self.line;
        #[cfg(test)]
        let reads = &self.max_reads;
        self.max_cache
            .query(front + lo as u64, front + hi as u64, |g| {
                #[cfg(test)]
                reads.set(reads.get() + 1);
                line[(g - front) as usize].1
            })
    }

    /// Test-only reference: the original zero-seeded fold over the window.
    #[cfg(test)]
    fn scan_max(&self, lo: usize, hi: usize) -> f32 {
        let mut m = 0.0f32;
        for i in lo..=hi {
            self.max_reads.set(self.max_reads.get() + 1);
            m = m.max(self.line[i].1);
        }
        m
    }

    /// Compute one hop's actual evidence under the currently effective
    /// `h` (MAN-190: actual computation, no forward prediction). Sets
    /// only the periodic/gate BASE anchor; the sign-crossing anchor is
    /// decided later from two real hops and must never depend on state a
    /// caller can change between computations.
    fn compute(&mut self, center: usize) -> HopEvidence {
        let (amp, _, n, ts) = self.line[center];
        // Centered maximum of the smoothed amplitude over [center-h, center+h],
        // clipped to the retained samples (SPEC v2 §1.2).
        let lo = center.saturating_sub(self.h);
        let hi = (center + self.h).min(self.line.len() - 1);
        let m = self.centered_max(lo, hi);
        let present = m >= 2.0 * n;
        let llr = if present {
            let u = ((amp - n) / (m - n).max(1e-9)).clamp(-0.5, 1.5);
            ((u - 0.5) / (self.cfg.sigma_u * self.cfg.sigma_u))
                .clamp(-self.cfg.llr_clip, self.cfg.llr_clip)
        } else {
            -self.cfg.llr_clip
        };
        // Base anchors only (SPEC v2 §1.7): periodic fallback cadence and
        // gate transitions. `last_present` starts false, so the first
        // gated-on hop anchors exactly like any other gate rise.
        let anchor = self.hop_out.is_multiple_of(self.cfg.fallback_hops as u64)
            || present != self.last_present;
        self.last_present = present;
        self.prefix += llr as f64;
        let ev = HopEvidence {
            hop: self.hop_out,
            sample_ts: ts,
            llr,
            present,
            anchor,
            prefix: self.prefix,
            amp,
            mark_level: m,
            noise_level: n,
        };
        self.hop_out += 1;
        self.next_center_g += 1;
        ev
    }

    /// Finalize the pending hop against its actual successor and take the
    /// successor's place in the pending slot (MAN-190's causal order).
    ///
    /// Edge trigger (SPEC v2 §1.7): a hop is an anchor at a half-amplitude
    /// crossing. The crossing sample itself is ambiguous between "last hop
    /// of the old sign" and "first hop of the new sign" (the emulated
    /// channel ramp puts the true crossing exactly between two discrete
    /// hops); tagging the LATER hop pushes every measured edge one hop
    /// past the true keying edge system-atically. Anchoring the EARLIER
    /// hop keeps edges anchored to within one hop of the true edge at any
    /// keying depth, at the cost of holding each hop until its successor
    /// is computed. Both LLRs here are values the callers actually
    /// received/will receive -- there is no prediction to go stale when
    /// `set_u_ref` changes `h` between computations, which is exactly the
    /// race PR #161 rounds 13 and 16 kept finding in the old lookahead.
    /// The three-way zero rule matters: an exact 0 LLR differs from both
    /// +1 and -1 signs (`f32::signum` alone does not implement this).
    fn finalize_pending(&mut self, successor: HopEvidence) -> HopEvidence {
        let mut earlier = self.pending.take().expect("pending hop to finalize");
        if sign_of(earlier.llr) != sign_of(successor.llr) {
            earlier.anchor = true;
        }
        self.pending = Some(successor);
        earlier
    }
}

/// Three-way sign: -1 for negative, 0 for exactly zero (including -0.0),
/// +1 for positive.
fn sign_of(x: f32) -> i8 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// SPEC v2 §1.2's centered maximum `M`, maintained as a monotonic deque
/// keyed by global (push-order) input index (MAN-193).
///
/// Invariant after every [`query`](Self::query): `cand` holds, in
/// increasing index order and strictly decreasing value order, every
/// sample in the queried interval that no later sample in that interval
/// equals or exceeds, and only positive non-NaN values. Its front is
/// therefore the interval's maximum, and an empty queue is the scan's 0.0
/// floor. A dominated sample can be discarded for good while both bounds
/// move forward: the later, larger sample that dominated it leaves the
/// window no earlier than it does. When a bound retreats (a `set_u_ref`
/// that widens the window's left side or narrows its right side), a
/// discarded sample can become the maximum again, so the queue is rebuilt
/// from the retained samples of the new interval.
///
/// Cost: with both bounds nondecreasing, each sample is admitted and
/// removed at most once, O(1) amortized per hop; repeated `set_u_ref` calls
/// that round to the same `h` change nothing. Each bound retreat costs one
/// rebuild of the new window, O(2h+1).
#[derive(Debug, Default)]
struct CenteredMax {
    /// `(global index, smoothed amplitude)` candidates.
    cand: VecDeque<(u64, f32)>,
    /// Inclusive global bounds of the last actual query.
    bounds: Option<(u64, u64)>,
    #[cfg(test)]
    ops: MaxOps,
}

/// Test-only operation counts, incremented at the operations themselves.
#[cfg(test)]
#[derive(Debug, Default)]
struct MaxOps {
    /// Samples offered to the queue, by any path.
    admitted: u64,
    /// Back-of-queue dominance comparisons.
    comparisons: u64,
    pushes: u64,
    /// Front expiries plus dominated back removals.
    pops: u64,
    /// Queue constructions, the initial one included.
    builds: u64,
    /// Samples admitted while rebuilding after a bound retreat.
    rebuilt_admissions: u64,
    /// Candidates read by [`CenteredMax::peek_next`].
    peek_inspections: std::cell::Cell<u64>,
    /// Entering samples read by [`CenteredMax::peek_next`].
    peek_reads: std::cell::Cell<u64>,
}

impl CenteredMax {
    /// Maximum of `value(g)` over the inclusive global interval `[lo, hi]`,
    /// floored at 0.0. `value` must accept every index in `[lo, hi]`;
    /// indices below `lo` and already-admitted indices are never read.
    fn query(&mut self, lo: u64, hi: u64, value: impl Fn(u64) -> f32) -> f32 {
        debug_assert!(lo <= hi, "empty centered-max window [{lo}, {hi}]");
        let start = match self.bounds {
            Some((prev_lo, prev_hi)) if lo >= prev_lo && hi >= prev_hi => {
                while let Some(&(g, _)) = self.cand.front() {
                    if g >= lo {
                        break;
                    }
                    self.cand.pop_front();
                    #[cfg(test)]
                    {
                        self.ops.pops += 1;
                    }
                }
                // `max(lo, ..)`: a window that jumped past the previous one
                // must not read the evicted gap between them.
                lo.max(prev_hi + 1)
            }
            _ => {
                #[cfg(test)]
                {
                    self.ops.builds += 1;
                    if self.bounds.is_some() {
                        self.ops.rebuilt_admissions += hi - lo + 1;
                    }
                }
                self.cand.clear();
                lo
            }
        };
        for g in start..=hi {
            self.admit(g, value(g));
        }
        self.bounds = Some((lo, hi));
        self.cand.front().map_or(0.0, |&(_, v)| v)
    }

    /// Append sample `g`, first dropping every back candidate it equals or
    /// exceeds. NaN and nonpositive values never beat the 0.0 floor (the
    /// scan's `f32::max` skips NaN), so they are not stored.
    fn admit(&mut self, g: u64, v: f32) {
        #[cfg(test)]
        {
            self.ops.admitted += 1;
        }
        if v.is_nan() || v <= 0.0 {
            return;
        }
        while let Some(&(_, back)) = self.cand.back() {
            #[cfg(test)]
            {
                self.ops.comparisons += 1;
            }
            if back > v {
                break;
            }
            self.cand.pop_back();
            #[cfg(test)]
            {
                self.ops.pops += 1;
            }
        }
        self.cand.push_back((g, v));
        #[cfg(test)]
        {
            self.ops.pushes += 1;
        }
    }

    /// Maximum over `[next_lo, next_hi]`, the window of the center one hop
    /// after the last query at the same `h`, without changing any state.
    /// That window loses at most the last query's leftmost sample
    /// (`next_lo` is `lo` or `lo + 1`) and gains at most one sample
    /// (`next_hi` is `hi` or `hi + 1`), so it needs at most two candidates
    /// and one read of `value(next_hi)`.
    ///
    /// Test-only since MAN-190: Evidence computes each center for real
    /// before using its maximum, so no production path peeks ahead.
    #[cfg(test)]
    fn peek_next(&self, next_lo: u64, next_hi: u64, value: impl Fn(u64) -> f32) -> f32 {
        let (lo, hi) = self.bounds.expect("peek_next follows a query");
        debug_assert!(
            next_lo == lo || next_lo == lo + 1,
            "next_lo {next_lo} vs lo {lo}"
        );
        debug_assert!(
            next_hi == hi || next_hi == hi + 1,
            "next_hi {next_hi} vs hi {hi}"
        );
        let mut cand = self.cand.iter();
        let inspect = || {
            let n = &self.ops.peek_inspections;
            n.set(n.get() + 1);
        };
        let kept = match cand.next() {
            Some(&(g, v)) if g >= next_lo => {
                inspect();
                v
            }
            Some(_) => {
                inspect();
                match cand.next() {
                    Some(&(_, v)) => {
                        inspect();
                        v
                    }
                    None => 0.0,
                }
            }
            None => 0.0,
        };
        if next_hi > hi {
            let n = &self.ops.peek_reads;
            n.set(n.get() + 1);
            kept.max(value(next_hi))
        } else {
            kept
        }
    }
}

/// MAN-193: helper-level tests of [`CenteredMax`] against an independent
/// zero-seeded `f32::max` fold over the same inclusive interval.
#[cfg(test)]
mod centered_max_tests {
    use super::*;

    /// The original Evidence maximum: a zero-seeded `f32::max` fold over the
    /// inclusive global interval `[lo, hi]`.
    fn scan_max(values: &[f32], lo: u64, hi: u64) -> f32 {
        let mut m = 0.0f32;
        for g in lo..=hi {
            m = m.max(values[g as usize]);
        }
        m
    }

    /// Deterministic xorshift amplitudes in `[0, 1)`.
    fn seeded(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 40) as f32 / (1u64 << 24) as f32
            })
            .collect()
    }

    fn shaped_streams(n: usize) -> Vec<(&'static str, Vec<f32>)> {
        vec![
            (
                "increasing",
                (0..n).map(|i| 0.1 + i as f32 * 1e-3).collect(),
            ),
            (
                "decreasing",
                (0..n).map(|i| 0.1 + (n - i) as f32 * 1e-3).collect(),
            ),
            ("constant", vec![0.5; n]),
            (
                "repeated-peaks",
                (0..n)
                    .map(|i| if i % 37 == 0 || i % 37 == 5 { 2.0 } else { 0.3 })
                    .collect(),
            ),
            (
                "isolated-peaks",
                (0..n)
                    .map(|i| if i % 611 == 300 { 9.0 } else { 0.2 })
                    .collect(),
            ),
            ("all-zero", vec![0.0; n]),
            ("seeded", seeded(n, 193)),
        ]
    }

    /// Reads `values[g]`, failing the test if `g` was evicted (before
    /// `front`) or not yet recorded (after `last`).
    fn reader(values: &[f32], front: u64, last: u64) -> impl Fn(u64) -> f32 + '_ {
        move |g| {
            assert!(
                g >= front && g <= last,
                "read global index {g} outside retained [{front}, {last}]"
            );
            values[g as usize]
        }
    }

    /// One Evidence-shaped computation: the actual query for center `c`
    /// at width `h` over the retained `[front, last]`, checked against the
    /// scan, followed (when `c + 1` is recorded) by the read-only adjacent
    /// peek for `c + 1`, checked against the scan and for immutability.
    fn step(cm: &mut CenteredMax, values: &[f32], c: u64, h: u64, front: u64, last: u64) -> f32 {
        let lo = front.max(c.saturating_sub(h));
        let hi = last.min(c + h);
        let m = cm.query(lo, hi, reader(values, front, last));
        let want = scan_max(values, lo, hi);
        assert_eq!(m.to_bits(), want.to_bits(), "center {c} h {h} [{lo}, {hi}]");
        assert!(
            cm.cand.len() as u64 <= hi - lo + 1,
            "candidate storage {} exceeds window length {}",
            cm.cand.len(),
            hi - lo + 1
        );
        if c < last {
            let next_lo = front.max((c + 1).saturating_sub(h));
            let next_hi = last.min(c + 1 + h);
            let before: Vec<(u64, f32)> = cm.cand.iter().copied().collect();
            let bounds = cm.bounds;
            let p1 = cm.peek_next(next_lo, next_hi, reader(values, front, last));
            let p2 = cm.peek_next(next_lo, next_hi, reader(values, front, last));
            let want_next = scan_max(values, next_lo, next_hi);
            assert_eq!(p1.to_bits(), want_next.to_bits(), "peek after center {c}");
            assert_eq!(p1.to_bits(), p2.to_bits(), "repeated peeks disagree");
            let after: Vec<(u64, f32)> = cm.cand.iter().copied().collect();
            assert_eq!(before, after, "peek mutated the candidates");
            assert_eq!(bounds, cm.bounds, "peek mutated the stored bounds");
        }
        m
    }

    /// Evidence's fixed-width schedule over `values`: center `c` is
    /// computed once `c + h` is recorded (live) or at EOF, with nothing
    /// trimmed. Returns the number of peeks performed (`step` peeks twice
    /// per center to check that peeking is read-only).
    fn fixed_schedule(cm: &mut CenteredMax, values: &[f32], h: u64) -> u64 {
        let n = values.len() as u64;
        let mut peeks = 0;
        for c in 0..n {
            let last = (n - 1).min(c + h);
            step(cm, values, c, h, 0, last);
            if c < last {
                peeks += 2;
            }
        }
        peeks
    }

    #[test]
    fn fixed_windows_match_the_scan_with_work_independent_of_h() {
        let n = 5000usize;
        for (name, values) in shaped_streams(n) {
            for h in [1u64, 60, 224, 1024] {
                let mut cm = CenteredMax::default();
                let peeks = fixed_schedule(&mut cm, &values, h);
                let ops = &cm.ops;
                let n = n as u64;
                assert_eq!(ops.builds, 1, "{name} h={h}: fixed window rebuilt");
                assert_eq!(ops.rebuilt_admissions, 0, "{name} h={h}");
                assert!(
                    ops.admitted <= n,
                    "{name} h={h}: {} admissions",
                    ops.admitted
                );
                assert!(ops.pushes <= n, "{name} h={h}: {} pushes", ops.pushes);
                assert!(ops.pops <= n, "{name} h={h}: {} removals", ops.pops);
                assert!(
                    ops.comparisons <= 2 * n,
                    "{name} h={h}: {} back comparisons",
                    ops.comparisons
                );
                assert!(ops.peek_inspections.get() <= 2 * peeks, "{name} h={h}");
                assert!(ops.peek_reads.get() <= peeks, "{name} h={h}");
            }
        }
    }

    #[test]
    fn startup_clipping_eof_clipping_and_empty_queues_match_the_scan() {
        // All-zero and negative-only windows leave the candidate queue empty
        // and must read as the scan's 0.0 floor.
        let mut values = vec![0.0f32; 40];
        values.extend([-1.0f32; 40]);
        values.extend([0.5f32, 0.25, 0.75]);
        let h = 6u64;
        let mut cm = CenteredMax::default();
        let n = values.len() as u64;
        for c in 0..n {
            let m = step(&mut cm, &values, c, h, 0, (n - 1).min(c + h));
            if c + h < 40 {
                assert_eq!(m, 0.0);
                assert!(cm.cand.is_empty(), "nonpositive values entered the queue");
            }
        }
    }

    #[test]
    fn disjoint_forward_windows_skip_unread_samples() {
        let values = seeded(400, 7);
        let mut cm = CenteredMax::default();
        // Each window starts past the previous one's end; the reader allows
        // only the current window, so nothing in a gap may be read.
        for (lo, hi) in [(0u64, 10u64), (20, 25), (26, 26), (100, 180), (300, 399)] {
            let m = cm.query(lo, hi, reader(&values, lo, hi));
            assert_eq!(m.to_bits(), scan_max(&values, lo, hi).to_bits());
        }
        assert_eq!(cm.ops.builds, 1);
    }

    #[test]
    fn width_growth_restores_an_excluded_left_peak() {
        let mut values = vec![0.2f32; 200];
        values[50] = 3.0;
        let mut cm = CenteredMax::default();
        // h = 5 around center 70 excludes the peak at 50 ...
        let small = step(&mut cm, &values, 70, 5, 0, 199);
        assert_eq!(small, 0.2);
        // ... and growing to h = 30 at center 71 retreats the left bound.
        let builds = cm.ops.builds;
        let grown = step(&mut cm, &values, 71, 30, 0, 199);
        assert_eq!(grown, 3.0, "the restored left peak must set the maximum");
        assert_eq!(
            cm.ops.builds,
            builds + 1,
            "a left-bound retreat rebuilds once"
        );
        assert_eq!(cm.ops.rebuilt_admissions, (101 - 41 + 1) as u64);
    }

    #[test]
    fn width_shrink_removes_a_dominating_right_peak() {
        let mut values = vec![0.2f32; 200];
        values[90] = 3.0;
        values[75] = 1.0;
        let mut cm = CenteredMax::default();
        // h = 20 around center 70 includes the peak at 90 (which dominated
        // the 1.0 at 75 out of the queue) ...
        assert_eq!(step(&mut cm, &values, 70, 20, 0, 199), 3.0);
        assert!(
            !cm.cand.iter().any(|&(g, _)| g == 75),
            "75 must be dominated"
        );
        // ... shrinking to h = 6 at center 71 retreats the right bound below
        // 90; the dominated 1.0 must come back.
        let builds = cm.ops.builds;
        assert_eq!(step(&mut cm, &values, 71, 6, 0, 199), 1.0);
        assert_eq!(
            cm.ops.builds,
            builds + 1,
            "a right-bound retreat rebuilds once"
        );
    }

    #[test]
    fn width_changes_with_advancing_bounds_never_rebuild() {
        let values = seeded(3000, 11);
        let mut cm = CenteredMax::default();
        let mut h = 60u64;
        // One-hop changes between consecutive centers: h+1 keeps lo and
        // advances hi by 2; h-1 advances lo by 2 and keeps hi. Neither bound
        // retreats, so the cache keeps maintaining incrementally.
        for c in 100..2800u64 {
            h = match c % 7 {
                0..=2 => h + 1,
                3..=5 => h - 1,
                _ => h,
            };
            step(&mut cm, &values, c, h, 0, 2999);
        }
        assert_eq!(cm.ops.builds, 1, "advancing bounds must not rebuild");
        assert!(cm.ops.admitted <= 3000);
    }

    #[test]
    fn backward_bounds_rebuild_once_each_and_match_the_scan() {
        let values = seeded(14_000, 193);
        // Alternating 223/225 retreats one bound on every query.
        let mut cm = CenteredMax::default();
        let mut rebuilt = 0u64;
        let mut prev: Option<(u64, u64)> = None;
        for c in 300..10_300u64 {
            let h = if c % 2 == 0 { 223 } else { 225 };
            let (lo, hi) = (c - h, c + h);
            if let Some((plo, phi)) = prev {
                if lo < plo || hi < phi {
                    rebuilt += hi - lo + 1;
                }
            }
            prev = Some((lo, hi));
            step(&mut cm, &values, c, h, 0, 13_999);
        }
        assert_eq!(
            cm.ops.builds, 10_000,
            "every 223/225 alternation retreats a bound"
        );
        assert_eq!(cm.ops.rebuilt_admissions, rebuilt);
        // Seeded large jumps between 1, 4, 60, 224 and 1024.
        let widths = [1u64, 4, 60, 224, 1024];
        let mut pick = seeded(10_000, 5).into_iter();
        let mut cm = CenteredMax::default();
        for c in 1100..10_100u64 {
            let h = widths[(pick.next().unwrap() * 5.0) as usize % 5];
            step(&mut cm, &values, c, h, 0, 13_999);
        }
        assert!(cm.ops.builds > 1);
    }

    #[test]
    fn global_indices_survive_retention_trimming() {
        // A long stream whose retained origin moves past MAX_RETAIN several
        // times; the reader rejects any evicted index.
        let n = 3 * MAX_RETAIN + 777;
        let values = seeded(n, 3);
        let mut cm = CenteredMax::default();
        let h = 224u64;
        let mut c = 0u64;
        for recorded in 1..=n as u64 {
            let front = recorded.saturating_sub(MAX_RETAIN as u64);
            while c + h < recorded {
                step(&mut cm, &values, c, h, front, recorded - 1);
                c += 1;
            }
            if recorded == 2 * MAX_RETAIN as u64 {
                // A width change near a retention rollover.
                for _ in 0..3 {
                    step(&mut cm, &values, c, 60, front, recorded - 1);
                    c += 1;
                }
            }
        }
        assert!(cm.cand.len() <= 2 * h as usize + 1);
        assert_eq!(
            cm.ops.builds, 3,
            "only the initial build, the shrink and the regrowth build"
        );
    }

    #[test]
    fn adjacent_peek_covers_expiry_second_candidate_entry_and_clipping() {
        let mut cm = CenteredMax::default();
        // Front candidate expires, a second candidate exists.
        let v = [5.0f32, 1.0, 2.0, 0.5, 0.1, 0.1];
        assert_eq!(cm.query(0, 3, reader(&v, 0, 5)), 5.0);
        assert_eq!(cm.peek_next(1, 3, reader(&v, 0, 5)), 2.0);
        // Front expires and no second candidate exists.
        let mut cm = CenteredMax::default();
        let z = [5.0f32, 0.0, 0.0, 0.0];
        assert_eq!(cm.query(0, 2, reader(&z, 0, 3)), 5.0);
        assert_eq!(cm.peek_next(1, 2, reader(&z, 0, 3)), 0.0);
        // A new right peak enters.
        assert_eq!(cm.peek_next(1, 3, reader(&[5.0, 0.0, 0.0, 7.0], 0, 3)), 7.0);
        // Right boundary clipped at EOF: nothing enters, nothing is read.
        let mut cm = CenteredMax::default();
        let w = [1.0f32, 3.0, 2.0];
        assert_eq!(cm.query(1, 2, reader(&w, 0, 2)), 3.0);
        assert_eq!(cm.peek_next(1, 2, reader(&w, 1, 2)), 3.0);
        assert_eq!(cm.ops.peek_reads.get(), 0);
        assert_eq!(cm.bounds, Some((1, 2)));
    }

    #[test]
    fn float_policy_matches_the_zero_seeded_fold() {
        let nan = f32::NAN;
        let cases: Vec<Vec<f32>> = vec![
            vec![nan, 0.5, nan, 0.25],
            vec![nan, nan, nan],
            vec![0.5, f32::INFINITY, 0.25, nan],
            vec![f32::NEG_INFINITY, -2.0, -0.5],
            vec![-0.0, 0.0, -0.0],
            vec![0.0, -0.0],
            vec![1.0, 1.0, 1.0],
        ];
        for values in cases {
            let n = values.len() as u64;
            let mut cm = CenteredMax::default();
            for c in 0..n {
                let lo = c.saturating_sub(1);
                let hi = (n - 1).min(c + 1);
                let m = cm.query(lo, hi, reader(&values, 0, n - 1));
                let want = scan_max(&values, lo, hi);
                assert!(!m.is_nan(), "{values:?}: NaN became the maximum");
                if want == 0.0 {
                    // Either signed zero is a valid f32::max result.
                    assert_eq!(m, 0.0, "{values:?} [{lo}, {hi}]");
                } else {
                    assert_eq!(m.to_bits(), want.to_bits(), "{values:?} [{lo}, {hi}]");
                }
                if c + 1 < n {
                    // Center c + 1 at h = 1 spans [c, c + 2], clipped.
                    let next_hi = (n - 1).min(c + 2);
                    let p = cm.peek_next(c, next_hi, reader(&values, 0, n - 1));
                    let want = scan_max(&values, c, next_hi);
                    if want == 0.0 {
                        assert_eq!(p, 0.0);
                    } else {
                        assert_eq!(p.to_bits(), want.to_bits());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangular keyed envelope at `depth_db`, one dit = `dit_hops`,
    /// pattern "dit gap dah gap" repeated, with the SPEC §1.2 channel filter
    /// emulated by a 4-hop linear ramp on each edge.
    fn keyed(dit_hops: usize, depth_db: f32, reps: usize) -> Vec<f32> {
        let lo = 10f32.powf(-depth_db / 20.0);
        let mut v = Vec::new();
        let seg = |v: &mut Vec<f32>, on: bool, n: usize| {
            for _ in 0..n {
                v.push(if on { 1.0 } else { lo });
            }
        };
        for _ in 0..reps {
            seg(&mut v, true, dit_hops);
            seg(&mut v, false, dit_hops);
            seg(&mut v, true, 3 * dit_hops);
            seg(&mut v, false, 3 * dit_hops);
        }
        // 4-hop ramps
        let mut out = v.clone();
        for i in 1..v.len() {
            if (v[i] - v[i - 1]).abs() > 0.1 {
                for j in 0..4 {
                    let k = i + j;
                    if k < out.len() {
                        let f = (j as f32 + 0.5) / 4.0;
                        out[k] = v[i - 1] + f * (v[i] - v[i - 1]);
                    }
                }
            }
        }
        out
    }

    fn run(env: &[f32], noise_amp: f32) -> Vec<HopEvidence> {
        run_with_config(env, noise_amp, EvidenceConfig::default())
    }

    fn run_with_config(env: &[f32], noise_amp: f32, cfg: EvidenceConfig) -> Vec<HopEvidence> {
        let mut e = Evidence::new(cfg);
        let mut out = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            out.extend(e.push(a, noise_amp, i as u64 * 512));
        }
        out.extend(e.flush());
        out
    }

    #[test]
    fn sign_edge_catches_a_transition_the_old_stale_center_levels_missed() {
        // Codex review, PR #161 round 13: the lookahead sign prediction
        // must use the SAME (m, n) pair center+1's own `emit()` call will
        // actually use, not center's stale values. This is a direct,
        // empirically-confirmed repro (found by running the code, not
        // hand-derived): a small window (h=1) with a steady high baseline
        // (always `present`) followed by a brief dip reliably produces an
        // unanchored sign flip under the old center-levels formula, since
        // the dip's own centered-max window differs from the levels used
        // to predict it one hop earlier.
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 1.0, // h = round(1.0 * 1.0) = 1
            sigma_u: 0.1,     // sharpen the LLR so small u shifts flip sign cleanly
            ..EvidenceConfig::default()
        };
        let mut e = Evidence::new(cfg);
        let n = 1.0f32;
        let baseline = 2.5f32;
        let mut out = Vec::new();
        for i in 0..50u64 {
            out.extend(e.push(baseline, n, i * 512));
        }
        for i in 50..53u64 {
            out.extend(e.push(1.6, n, i * 512));
        }
        for i in 53..56u64 {
            out.extend(e.push(baseline, n, i * 512));
        }
        let sign = |llr: f32| -> i8 {
            if llr > 0.0 {
                1
            } else if llr < 0.0 {
                -1
            } else {
                0
            }
        };
        for w in out.windows(2) {
            if sign(w[0].llr) != sign(w[1].llr) {
                assert!(
                    w[0].anchor,
                    "hop {} -> {} sign changed ({} -> {}) but the earlier hop wasn't anchored",
                    w[0].hop, w[1].hop, w[0].llr, w[1].llr
                );
            }
        }
    }

    #[test]
    fn sign_edge_anchors_every_genuine_sign_transition_between_emitted_hops() {
        // Codex review, PR #161 round 13: the lookahead sign prediction
        // must use the SAME (m, n) pair center+1's own `emit()` call will
        // actually use, not center's stale values -- the centered-max
        // window shifts by one position between adjacent centers, so a
        // mismatch here can predict a sign the next center's own real
        // computation never reaches, missing a genuine current-to-next
        // crossing permanently. Verify directly: for every pair of
        // consecutively emitted hops whose LLR sign differs, the earlier
        // hop must be flagged as an anchor.
        let dit_hops = 13usize;
        let env = keyed(dit_hops, 45.0, 20);
        let noise_amp = 10f32.powf(-45.0 / 20.0) * 0.5;
        let evs = run(&env, noise_amp);
        let sign = |llr: f32| -> i8 {
            if llr > 0.0 {
                1
            } else if llr < 0.0 {
                -1
            } else {
                0
            }
        };
        for w in evs.windows(2) {
            if sign(w[0].llr) != sign(w[1].llr) {
                assert!(
                    w[0].anchor,
                    "hop {} -> {} sign changed ({} -> {}) but the earlier hop wasn't anchored",
                    w[0].hop, w[1].hop, w[0].llr, w[1].llr
                );
            }
        }
    }

    // [RULING, SDD execution 2026-09-09, Task 4 fix round 1]: this test as
    // originally drafted read `.last()` deep into each 200-hop probe phase,
    // by which point M (a *local*, QSB-tracking window per SPEC v2 §1.2, not
    // a global peak-hold) has fully re-converged to equal the probe's own
    // amplitude -- degenerating u_amp to exactly 1.0 for *any* held probe
    // value and making both readings same-sign (verified: sum ~= +11.1, not
    // antisymmetric). The fix probes the LLR formula's antisymmetry directly
    // during the genuine transient right after a probe amplitude changes
    // (while the window still spans back into the earlier high plateau,
    // `mark_level > 0.99`), using noise_amp=0.0 so the antisymmetry midpoint
    // lands exactly at 0.5 with no algebraic offset from a nonzero floor.
    #[test]
    fn llr_is_antisymmetric_about_half_amplitude() {
        let mut e = Evidence::new(EvidenceConfig::default());
        let mut out = Vec::new();
        for _ in 0..300 {
            out.extend(e.push(1.0, 0.0, 0));
        }
        out.extend(e.push(0.75, 0.0, 0));
        out.extend(e.push(0.25, 0.0, 0));
        for _ in 0..200 {
            out.extend(e.push(0.75, 0.0, 0));
        }
        out.extend(e.flush());
        let hi = out
            .iter()
            .find(|h| (h.amp - 0.75).abs() < 1e-6 && h.mark_level > 0.99)
            .unwrap()
            .llr;
        let lo = out
            .iter()
            .find(|h| (h.amp - 0.25).abs() < 1e-6 && h.mark_level > 0.99)
            .unwrap()
            .llr;
        assert!((hi + lo).abs() < 0.05, "hi {hi} lo {lo}");
        assert!(hi > 0.0);
    }

    // [RULING, SDD execution 2026-09-09, Task 4 fix round 1]: as originally
    // drafted this test counted every `anchor` hop, including ones triggered
    // only by the periodic `fallback_hops` mechanism (SPEC v2 §1.7) -- which
    // exists to bound the future HSMM decoder's per-anchor reach during long
    // unbroken runs and is *not* meant to land near a true keying edge.
    // With the production default `fallback_hops=8` against this scene's
    // 13/39-hop element lengths, most fallback phases are structurally
    // off-edge (verified by hand: 11 of 13 residues mod 104), which is by
    // design, not a bug -- confirmed separately that with fallback anchors
    // excluded, sign-transition/present-transition ("organic") anchors are
    // 81 total with 1 off-target (the tail present->absent flip; still well
    // inside this test's 5% tolerance) at all three depths. The fix gives
    // this test its own
    // `EvidenceConfig` with `fallback_hops` far longer than the whole scene,
    // isolating exactly the property this test is meant to check.
    #[test]
    fn anchors_land_on_true_edges_regardless_of_depth() {
        for depth in [20.0f32, 40.0, 60.0] {
            let env = keyed(13, depth, 20);
            let cfg = EvidenceConfig {
                fallback_hops: 1_000_000,
                ..EvidenceConfig::default()
            };
            let hops = run_with_config(&env, 10f32.powf(-depth / 20.0), cfg);
            let anchors: Vec<u64> = hops
                .iter()
                .filter(|h| h.anchor && h.llr.signum() != 0.0)
                .map(|h| h.hop)
                .collect();
            // true edges of the "dit gap dah gap" pattern (period 8 dits) start at 0 in the un-ramped envelope;
            // each measured edge must be within 1 hop of a true edge at 13k or 13k+... boundaries.
            let mut off_by = 0;
            for &a in &anchors {
                let m = a % (8 * 13);
                let nearest = [0u64, 13, 26, 65]
                    .iter()
                    .map(|&t| {
                        (m as i64 - t as i64)
                            .abs()
                            .min(104 - (m as i64 - t as i64).abs())
                    })
                    .min()
                    .unwrap();
                if nearest > 1 {
                    off_by += 1;
                }
            }
            assert!(
                off_by * 20 < anchors.len(),
                "depth {depth}: {off_by}/{} anchors more than 1 hop from a true edge",
                anchors.len()
            );
        }
    }

    #[test]
    fn segment_sums_measure_true_durations_at_60_db() {
        let env = keyed(13, 60.0, 10);
        let hops = run(&env, 0.001);
        // Count consecutive positive-llr runs after warm-up: dits must be 13±1, dahs 39±1.
        let mut runs = Vec::new();
        let mut cur = 0;
        let mut sign = false;
        for h in hops.iter().skip(200) {
            let s = h.llr > 0.0;
            if s == sign {
                cur += 1;
            } else {
                if sign {
                    runs.push(cur);
                }
                cur = 1;
                sign = s;
            }
        }
        let dits = runs.iter().filter(|&&r| (12..=14).contains(&r)).count();
        let dahs = runs.iter().filter(|&&r| (38..=40).contains(&r)).count();
        assert!(dits >= 8 && dahs >= 8, "runs {runs:?}");
    }

    #[test]
    fn gate_off_when_keying_depth_under_6_db() {
        let mut e = Evidence::new(EvidenceConfig::default());
        let last = (0..600).flat_map(|_| e.push(1.0, 0.8, 0)).last().unwrap();
        assert!(!last.present);
        assert_eq!(last.llr, -EvidenceConfig::default().llr_clip);
    }

    /// sign(x) with the three-way rule SPEC v2 §1.7 needs: -1 / 0 / +1.
    /// `f32::signum` alone does not implement the zero case (it returns
    /// +/-1 for +/-0.0 and 0 only for +0.0).
    fn sign3(x: f32) -> i8 {
        if x > 0.0 {
            1
        } else if x < 0.0 {
            -1
        } else {
            0
        }
    }

    /// MAN-190: `push_hop_hsmm`'s `best_u()` -> `set_u_ref()` feedback can
    /// change the hold window `h` between two adjacent hops' computations.
    /// The old forward-looking sign prediction ran inside `emit(center)`
    /// using whatever `h` held at that moment, so a window change landing
    /// between predicting center+1's sign and actually computing it made
    /// the prediction diverge from the real value -- silently dropping
    /// the anchor on a genuine mark/space crossing.
    ///
    /// Scene (from the ticket's reproduction): a steady mark plateau
    /// (amp 2.5, noise 1.0, always gated present) with a short spike
    /// (amp 20) at input hops 66..=68 -- just outside hop 61's small
    /// (h=4) centered-max reach, well inside its large (h=20) reach.
    /// Draining one hop at a time and growing the window exactly after
    /// returned hop 60 (what the per-hop feedback loop does) must still
    /// flag the earlier hop of every REAL sign crossing as an anchor.
    #[test]
    fn man_190_actual_crossing_with_window_feedback() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,         // h = round(1.0 * 4.0) = 4
            fallback_hops: 1_000_000, // disable periodic anchors: isolate sign_edge
            ..EvidenceConfig::default()
        };
        let mut e = Evidence::new(cfg);
        let n = 1.0f32;
        let plateau = 2.5f32;
        let spike = 20.0f32;
        let total = 150u64;
        for i in 0..total {
            let amp = if (66..=68).contains(&i) {
                spike
            } else {
                plateau
            };
            e.record(amp, n, i * 512);
        }
        let mut out = Vec::new();
        while let Some(ev) = e.drain_one() {
            if ev.hop == 60 {
                // Grow h 4 -> 20 exactly between two returned hops, the
                // same shape as push_hop_hsmm's per-hop speed feedback.
                e.set_u_ref(20.0);
            }
            out.push(ev);
        }
        out.extend(e.flush());
        assert_eq!(out.len(), total as usize, "one output per input");
        // The gate never closes near the crossing: `present` is constant
        // across the affected pair, so no gate-change anchor can hide a
        // missed crossing.
        for ev in out.iter().filter(|ev| (55..=70).contains(&ev.hop)) {
            assert!(ev.present, "hop {} unexpectedly gated off", ev.hop);
        }
        // A real positive-to-negative crossing must exist near the update
        // (the grown window pulls the spike into the next computed hop's
        // centered max, collapsing its llr toward the space side).
        let crossing_near_update = out
            .windows(2)
            .any(|w| (58..=66).contains(&w[0].hop) && sign3(w[0].llr) > 0 && sign3(w[1].llr) < 0);
        assert!(
            crossing_near_update,
            "expected a real +/- sign crossing near the window update: llrs {:?}",
            out.iter().map(|e| e.llr).collect::<Vec<_>>()
        );
        // The invariant this ticket exists for: EVERY real sign crossing
        // anchors its earlier hop -- including the one straddling the
        // window change. Not restricted to the specific pair, so the test
        // does not depend on the computation schedule.
        for w in out.windows(2) {
            if sign3(w[0].llr) != sign3(w[1].llr) {
                assert!(
                    w[0].anchor,
                    "hop {} -> {} sign changed ({} -> {}) but the earlier hop wasn't anchored",
                    w[0].hop, w[1].hop, w[0].llr, w[1].llr
                );
            }
        }
    }

    /// MAN-190, reverse direction: shrinking the window mid-drain removes
    /// the spike from the next computed hop's centered max, flipping its
    /// real evidence from space-like back to mark-like. The earlier hop of
    /// that crossing must be anchored too.
    #[test]
    fn man_190_actual_crossing_with_window_shrink_feedback() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 20.0, // h = round(1.0 * 20.0) = 20
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let mut e = Evidence::new(cfg);
        let n = 1.0f32;
        let plateau = 2.5f32;
        let spike = 20.0f32;
        let total = 150u64;
        for i in 0..total {
            let amp = if (66..=68).contains(&i) {
                spike
            } else {
                plateau
            };
            e.record(amp, n, i * 512);
        }
        let mut out = Vec::new();
        while let Some(ev) = e.drain_one() {
            if ev.hop == 60 {
                // Shrink h 20 -> 4: hop 60 was computed against the wide
                // window (spike inside, space-like evidence); the next
                // computed hop's narrow window excludes the spike.
                e.set_u_ref(4.0);
            }
            out.push(ev);
        }
        out.extend(e.flush());
        assert_eq!(out.len(), total as usize, "one output per input");
        for ev in out.iter().filter(|ev| (55..=70).contains(&ev.hop)) {
            assert!(ev.present, "hop {} unexpectedly gated off", ev.hop);
        }
        // A real crossing must exist near the update: the wide-window hops
        // around 60 read space-like (spike inflates the mark level), and
        // the hops covering the spike itself read mark-like.
        let crossing_near_update = out
            .windows(2)
            .any(|w| (58..=68).contains(&w[0].hop) && sign3(w[0].llr) != sign3(w[1].llr));
        assert!(
            crossing_near_update,
            "expected a real sign crossing near the window update: llrs {:?}",
            out.iter().map(|e| e.llr).collect::<Vec<_>>()
        );
        for w in out.windows(2) {
            if sign3(w[0].llr) != sign3(w[1].llr) {
                assert!(
                    w[0].anchor,
                    "hop {} -> {} sign changed ({} -> {}) but the earlier hop wasn't anchored",
                    w[0].hop, w[1].hop, w[0].llr, w[1].llr
                );
            }
        }
    }

    /// MAN-190 causal order: with one computed hop held pending until its
    /// successor's actual sign is known, a constant hold window delays the
    /// first release by h+1 input hops (h for the centered-max lookahead
    /// plus one for the confirming successor), and the first released hop
    /// is the stream's first input.
    #[test]
    fn delay_equals_hold_window_plus_one() {
        let mut e = Evidence::new(EvidenceConfig::default());
        let h = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops)
            .round() as usize;
        for i in 0..=h {
            assert!(
                e.push(1.0, 0.01, i as u64).is_empty(),
                "input {i} emitted with less than h+1 hops of lookahead"
            );
        }
        let out = e.push(1.0, 0.01, (h + 1) as u64);
        assert!(!out.is_empty(), "first output must arrive at input h+1");
        assert_eq!(out[0].hop, 0, "the first released hop must be hop 0");
    }

    /// MAN-190: a window update must never rewrite a hop Evidence has
    /// already computed. The pending hop's llr/mark level/prefix are
    /// frozen at computation time; only not-yet-computed centers see the
    /// new window. Verified against a reference run over the identical
    /// input with the window never changed: the two streams agree
    /// field-for-field through the update, then diverge exactly where the
    /// new window first applies (hop 62 onward).
    #[test]
    fn set_u_ref_cannot_change_a_computed_pending_hop() {
        let mk = || {
            Evidence::new(EvidenceConfig {
                hold_dits: 1.0,
                u_init_hops: 4.0,
                fallback_hops: 1_000_000,
                ..EvidenceConfig::default()
            })
        };
        let scene = |i: u64| -> f32 {
            if (66..=68).contains(&i) {
                20.0
            } else {
                2.5
            }
        };
        let total = 200u64;

        // Reference: constant h = 4 over the same input.
        let mut r = mk();
        let mut ref_out = Vec::new();
        // Live run: grow h 4 -> 20 right after returned hop 60.
        let mut a = mk();
        let mut out = Vec::new();
        let mut saw_stall = false;
        for i in 0..total {
            let (amp, n) = (scene(i), 1.0f32);
            a.record(amp, n, i * 512);
            r.record(amp, n, i * 512);
            while let Some(ev) = a.drain_one() {
                let just_returned_60 = ev.hop == 60;
                out.push(ev);
                if just_returned_60 {
                    a.set_u_ref(20.0); // h 4 -> 20
                                       // The growth temporarily makes the next center
                                       // ineligible even though the pending hop (61)
                                       // is already computed: drain_one() reports "no
                                       // finalized hop available" while retaining it.
                    match a.drain_one() {
                        Some(ev) => out.push(ev),
                        None => saw_stall = true,
                    }
                }
            }
            while let Some(ev) = r.drain_one() {
                ref_out.push(ev);
            }
        }
        assert!(
            saw_stall,
            "expected the window growth to stall the next computation \
             (pending retained) right after the update"
        );
        while let Some(ev) = a.drain_one() {
            out.push(ev);
        }
        out.extend(a.flush());
        while let Some(ev) = r.drain_one() {
            ref_out.push(ev);
        }
        ref_out.extend(r.flush());
        assert_eq!(out.len(), total as usize);
        assert_eq!(ref_out.len(), total as usize);

        // Everything computed before the update -- in particular the
        // pending hop 61, computed in the same drain that returned 60 --
        // must be bit-identical to the constant-window reference.
        for i in 0..=61usize {
            assert_eq!(out[i].llr, ref_out[i].llr, "hop {i} llr changed");
            assert_eq!(out[i].mark_level, ref_out[i].mark_level, "hop {i} mark");
            assert_eq!(out[i].prefix, ref_out[i].prefix, "hop {i} prefix");
            assert_eq!(out[i].amp, ref_out[i].amp, "hop {i} amp");
            assert_eq!(out[i].sample_ts, ref_out[i].sample_ts, "hop {i} ts");
        }
        // Hop 62 is the first center computed under the grown window; its
        // centered max spans the spike, unlike the reference's narrow
        // window, and later hops diverge accordingly (e.g. hop 75's wide
        // window still covers the spike, the reference's does not).
        assert!(
            out[62].mark_level > ref_out[62].mark_level,
            "hop 62 (first under the new window) must see the wider max: {} vs {}",
            out[62].mark_level,
            ref_out[62].mark_level
        );
        assert_ne!(out[75].mark_level, ref_out[75].mark_level);
    }

    /// Run `Evidence` over `scene` with the caller's per-hop feedback
    /// schedule, exactly the way `push_hop_hsmm` drives it live: record one
    /// input, drain one finalized hop at a time, and after each RETURNED
    /// hop number call `feedback` and apply any window update before the
    /// next computation (MAN-190: that next computation is hop t+2; hop
    /// t+1 is already pending). `batched` records the whole scene first
    /// and then runs the same live-eligible one-at-a-time drain -- the
    /// same incremental schedule under a different record/drain
    /// interleave, NOT the EOF forward-clipped flush.
    fn run_scheduled(
        cfg: EvidenceConfig,
        scene: &[(f32, f32)],
        feedback: &dyn Fn(u64) -> Option<f32>,
        batched: bool,
    ) -> Vec<HopEvidence> {
        let mut e = Evidence::new(cfg);
        let mut out = Vec::new();
        let drain = |e: &mut Evidence, out: &mut Vec<HopEvidence>| {
            while let Some(ev) = e.drain_one() {
                let hop = ev.hop;
                out.push(ev);
                if let Some(u) = feedback(hop) {
                    e.set_u_ref(u);
                }
            }
        };
        for (i, &(amp, noise)) in scene.iter().enumerate() {
            e.record(amp, noise, i as u64 * 512);
            if !batched {
                drain(&mut e, &mut out);
            }
        }
        drain(&mut e, &mut out);
        // Release the terminal pending hop at EOF (base anchor only).
        while let Some(ev) = e.flush_one() {
            out.push(ev);
        }
        out
    }

    /// Independent oracle for a fully-drained output stream (MAN-190's
    /// desired end state): every input produces exactly one output, in
    /// order, with its own amp/timestamp; `prefix[t]` is the sequential
    /// sum through `llr[t]`; and each hop's anchor is exactly
    ///
    /// ```text
    /// base_anchor[t] = (t % fallback_hops == 0) OR (present[t] != present[t-1])
    /// anchor[t] = base_anchor[t] OR (sign(llr[t]) != sign(llr[t+1]))
    /// anchor[last] = base_anchor[last]          // no synthetic successor
    /// ```
    ///
    /// with `present[-1] = false` and the three-way zero sign rule.
    fn check_stream_invariants(out: &[HopEvidence], scene: &[(f32, f32)], cfg: &EvidenceConfig) {
        assert_eq!(out.len(), scene.len(), "one output per input");
        for (i, ev) in out.iter().enumerate() {
            assert_eq!(ev.hop, i as u64, "hop ids must be contiguous from 0");
            assert_eq!(ev.sample_ts, i as u64 * 512, "hop {i}: timestamp moved");
            assert!(
                (ev.amp - scene[i].0).abs() < 1e-6,
                "hop {i}: amp {} != fed {}",
                ev.amp,
                scene[i].0
            );
            assert_eq!(ev.noise_level, scene[i].1, "hop {i}: noise moved");
            if i > 0 {
                let delta = ev.prefix - out[i - 1].prefix;
                assert!(
                    (delta - ev.llr as f64).abs() < 1e-6,
                    "hop {i}: prefix delta {delta} != llr {}",
                    ev.llr
                );
            }
            let prev_present = if i == 0 { false } else { out[i - 1].present };
            let base =
                ev.hop.is_multiple_of(cfg.fallback_hops as u64) || ev.present != prev_present;
            let crossing = i + 1 < out.len() && sign3(ev.llr) != sign3(out[i + 1].llr);
            assert_eq!(
                ev.anchor,
                base || crossing,
                "hop {i}: anchor {} != formula (base {base}, crossing {crossing})",
                ev.anchor
            );
        }
    }

    /// MAN-190 lifecycle: growth and shrink between outputs, several
    /// changes while draining a backlog, and every newly computed center
    /// seeing the latest update -- verified through the mark level a
    /// window of a given width produces. The spike sits at hops 100..=102:
    /// hop 51's narrow (h=4) window excludes it, hop 52's grown (h=60)
    /// window spans it, and hop 53's shrunk (h=2) window excludes it
    /// again. The pending hop computed before each update must keep the
    /// narrow-window mark level (immutability).
    #[test]
    fn window_updates_between_drains_apply_to_the_next_computed_center_only() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene: Vec<(f32, f32)> = (0..160)
            .map(|i| {
                let amp = if (100..=102).contains(&i) { 20.0 } else { 2.5 };
                (amp, 1.0)
            })
            .collect();
        // Record the first 113 inputs (so a full h=60 forward window is
        // available for center 52), then drain with updates by returned hop.
        let mut e = Evidence::new(cfg.clone());
        let mut out = Vec::new();
        for (i, &(amp, noise)) in scene.iter().take(113).enumerate() {
            e.record(amp, noise, i as u64 * 512);
        }
        let updates: &[(u64, f32)] = &[(50, 60.0), (51, 2.0), (53, 6.0)];
        while let Some(ev) = e.drain_one() {
            if let Some(&(_, u)) = updates.iter().find(|(hop, _)| *hop == ev.hop) {
                e.set_u_ref(u);
            }
            out.push(ev);
        }
        for (i, &(amp, noise)) in scene.iter().enumerate().skip(113) {
            e.record(amp, noise, i as u64 * 512);
            while let Some(ev) = e.drain_one() {
                out.push(ev);
            }
        }
        out.extend(e.flush());
        check_stream_invariants(&out, &scene, &cfg);
        // hop 51: computed (pending) under h=4 BEFORE the growth to 60.
        assert!(
            (out[51].mark_level - 2.5).abs() < 1e-4,
            "pending hop 51 must keep its narrow-window mark level: {}",
            out[51].mark_level
        );
        // hop 52: first center computed under h=60 -- spike in reach.
        assert!(
            out[52].mark_level > 10.0,
            "hop 52 must see the grown window's spike: {}",
            out[52].mark_level
        );
        // hop 53: first center computed under the shrunk h=2.
        assert!(
            (out[53].mark_level - 2.5).abs() < 1e-4,
            "hop 53 must see the shrunk window immediately: {}",
            out[53].mark_level
        );
    }

    /// MAN-190 lifecycle: a window change alone never fabricates a sign
    /// anchor. With a stable-sign stream, periodic fallback disabled and
    /// no gate transitions after the first, the only anchor is hop 0's
    /// genuine gate rise.
    #[test]
    fn stable_signs_under_a_changing_window_invent_no_anchors() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene = vec![(2.5f32, 1.0f32); 120];
        let out = run_scheduled(
            cfg.clone(),
            &scene,
            &|hop| match hop {
                10 | 30 => Some(24.0), // growth
                20 | 40 => Some(2.0),  // shrink
                _ => None,
            },
            false,
        );
        check_stream_invariants(&out, &scene, &cfg);
        let anchors: Vec<u64> = out.iter().filter(|e| e.anchor).map(|e| e.hop).collect();
        assert_eq!(anchors, vec![0], "only the initial gate rise may anchor");
    }

    /// MAN-190 lifecycle: exact-zero LLR transitions. Constant plateaus
    /// keep a_s exact (EMA of a constant is that constant), so every hop
    /// of the 1.0-amp stretch whose centered window still spans the 2.0
    /// plateau normalizes to exactly u = (1.0-0)/(2.0-0) = 0.5, i.e.
    /// llr = 0 -- a signed-to-zero-to-signed transition through the exact
    /// decision surface. Expected anchors are derived from the returned
    /// actual values with the three-way sign rule: 0 differs from both
    /// +1 and -1.
    #[test]
    fn zero_llr_transition_anchors_follow_the_three_way_sign_rule() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 8.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let mut scene: Vec<(f32, f32)> = vec![(2.0, 0.0); 40];
        scene.extend(vec![(1.0, 0.0); 30]);
        scene.extend(vec![(2.0, 0.0); 40]);
        let out = run_scheduled(cfg.clone(), &scene, &|_| None, false);
        check_stream_invariants(&out, &scene, &cfg);
        let zero_hops: Vec<u64> = out.iter().filter(|e| e.llr == 0.0).map(|e| e.hop).collect();
        assert_eq!(
            zero_hops,
            (40..=47u64).collect::<Vec<_>>(),
            "the 1.0-amp hops whose window spans the 2.0 plateau read exactly 0"
        );
        assert!(sign3(out[39].llr) > 0 && sign3(out[48].llr) > 0);
        // + -> 0 and 0 -> + are real crossings under the three-way rule:
        // the earlier hop of each is anchored.
        assert!(out[39].anchor, "+ -> 0 crossing must anchor hop 39");
        assert!(out[47].anchor, "0 -> + crossing must anchor hop 47");
        // An interior zero-zero pair is NOT a crossing.
        assert!(!out[43].anchor, "interior zero hop must not be anchored");
    }

    /// MAN-190 lifecycle: per-hop noise changes flow through the gate and
    /// LLR (each buffered sample carries its own noise amp), with anchors
    /// still derived only from returned actual values.
    #[test]
    fn per_hop_noise_changes_anchor_from_actual_values() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 8.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene: Vec<(f32, f32)> = (0..120)
            .map(|i| {
                let noise = 1.0 + 0.35 * (i as f32 * 0.37).sin();
                let amp = 2.4 + 0.4 * (i as f32 * 0.11).sin();
                (amp, noise)
            })
            .collect();
        let out = run_scheduled(cfg.clone(), &scene, &|_| None, false);
        check_stream_invariants(&out, &scene, &cfg);
        // The scene genuinely mixes gate states and signs.
        assert!(out.iter().any(|e| !e.present) && out.iter().any(|e| e.present));
        assert!(
            out.windows(2).any(|w| sign3(w[0].llr) != sign3(w[1].llr)),
            "scene must contain a real sign crossing"
        );
    }

    /// MAN-190 lifecycle: gate rise/fall anchors the LATER hop; the sign
    /// crossing of a straddling pair anchors the EARLIER hop; periodic
    /// fallback anchors its cadence independently. Asserted separately so
    /// a periodic anchor landing on the same hop cannot mask a missed
    /// crossing.
    #[test]
    fn gate_transitions_and_periodic_fallback_anchor_separately_from_sign_crossings() {
        let cfg = EvidenceConfig::default(); // fallback_hops = 8
                                             // Alternating gated-off silence (amp well under 2*noise) and
                                             // marks, with in-mark amplitude dips that cross the half-amplitude
                                             // surface.
        let mut scene: Vec<(f32, f32)> = Vec::new();
        for block in 0..8 {
            let mark_amp = if block % 2 == 0 { 3.0 } else { 2.9 };
            scene.extend(vec![(0.5f32, 1.0f32); 15]); // gated off
            scene.extend(vec![(mark_amp, 1.0); 25]); // present, near 0.5 u
            scene.extend(vec![(1.6, 1.0); 10]); // present, space-like dip
        }
        let out = run_scheduled(cfg.clone(), &scene, &|_| None, false);
        check_stream_invariants(&out, &scene, &cfg);
        for w in out.windows(2) {
            if w[0].present != w[1].present {
                assert!(
                    w[1].anchor,
                    "gate change at hop {} -> {} must anchor the LATER hop",
                    w[0].hop, w[1].hop
                );
            }
            if sign3(w[0].llr) != sign3(w[1].llr) {
                assert!(
                    w[0].anchor,
                    "sign change at hop {} -> {} must anchor the EARLIER hop",
                    w[0].hop, w[1].hop
                );
            }
        }
        for ev in &out {
            if ev.hop.is_multiple_of(cfg.fallback_hops as u64) {
                assert!(ev.anchor, "periodic cadence hop {} must anchor", ev.hop);
            }
        }
    }

    /// MAN-190 lifecycle: empty, one-input, two-input and shorter-than-h
    /// streams. Flush returns every real input exactly once with its
    /// original timestamp and correct prefix deltas; repeated flush_one
    /// and a later flush() yield nothing more.
    #[test]
    fn short_streams_flush_exactly_once() {
        for n in [0usize, 1, 2, 10] {
            let cfg = EvidenceConfig::default(); // h = 60 > n
            let scene: Vec<(f32, f32)> = (0..n).map(|i| (1.5 + 0.01 * i as f32, 0.5)).collect();
            let mut e = Evidence::new(cfg.clone());
            for (i, &(amp, noise)) in scene.iter().enumerate() {
                e.record(amp, noise, i as u64 * 512);
                assert!(e.drain_one().is_none(), "{n}-input run emitted live");
            }
            let mut out = Vec::new();
            while let Some(ev) = e.flush_one() {
                out.push(ev);
            }
            assert!(e.flush_one().is_none(), "flush_one repeated past the end");
            assert!(e.flush().is_empty(), "flush repeated past the end");
            check_stream_invariants(&out, &scene, &cfg);
        }
    }

    /// MAN-190 lifecycle: a crossing between the last two hops anchors the
    /// penultimate hop; the terminal hop keeps only its own base anchor --
    /// no synthetic successor, no zero padding, no shortened backward
    /// window (its mark level still spans the full held window).
    #[test]
    fn final_pair_crossing_anchors_the_penultimate_hop_only() {
        let cfg = EvidenceConfig {
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        // A mark plateau that dips below the half-amplitude surface for
        // the final hop only; the wide (h=60) centered max stays held at
        // the plateau throughout.
        let mut scene: Vec<(f32, f32)> = vec![(2.5f32, 1.0f32); 119];
        scene.push((0.9, 1.0));
        let out = run_scheduled(cfg.clone(), &scene, &|_| None, false);
        check_stream_invariants(&out, &scene, &cfg);
        let (penultimate, terminal) = (&out[out.len() - 2], out.last().unwrap());
        assert!(
            sign3(penultimate.llr) != sign3(terminal.llr),
            "scene must cross sign across the final pair"
        );
        assert!(penultimate.anchor, "penultimate hop must be anchored");
        assert!(!terminal.anchor, "terminal hop keeps only its base anchor");
        assert!(
            (terminal.mark_level - 2.5).abs() < 1e-4,
            "terminal backward window must stay unshrunk: {}",
            terminal.mark_level
        );
    }

    /// MAN-190 lifecycle: the record/drain interleave must not change the
    /// incremental schedule. Feedback keyed by RETURNED hop number lands
    /// at the same computed center whether the caller drains after every
    /// record (the live path) or records everything first, because the
    /// update schedule is expressed in returned-hop numbers, not
    /// wall-clock records.
    #[test]
    fn record_drain_interleave_does_not_change_the_schedule() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene: Vec<(f32, f32)> = (0..200)
            .map(|i| {
                let amp = if (80..=82).contains(&i) { 20.0 } else { 2.5 };
                (amp, 1.0)
            })
            .collect();
        let feedback = |hop: u64| -> Option<f32> {
            match hop {
                20 | 60 => Some(18.0),
                40 => Some(2.0),
                _ => None,
            }
        };
        let live = run_scheduled(cfg.clone(), &scene, &feedback, false);
        let batched = run_scheduled(cfg.clone(), &scene, &feedback, true);
        assert_eq!(live.len(), batched.len());
        for (a, b) in live.iter().zip(batched.iter()) {
            assert_eq!(a.hop, b.hop);
            assert_eq!(a.llr, b.llr, "hop {}: schedule diverged", a.hop);
            assert_eq!(a.mark_level, b.mark_level, "hop {}", a.hop);
            assert_eq!(a.prefix, b.prefix, "hop {}", a.hop);
            assert_eq!(a.anchor, b.anchor, "hop {}", a.hop);
        }
    }

    /// MAN-190 lifecycle: a stream longer than `MAX_RETAIN` with ordinary
    /// draining and a small window keeps exact output identity across
    /// deque trimming.
    #[test]
    fn output_identity_survives_retention_trimming() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene: Vec<(f32, f32)> = (0..(MAX_RETAIN + 900))
            .map(|i| (0.8 + 0.5 * ((i as f32) * 0.021).sin(), 0.2))
            .collect();
        let mut e = Evidence::new(cfg.clone());
        let mut out = Vec::new();
        for (i, &(amp, noise)) in scene.iter().enumerate() {
            e.record(amp, noise, i as u64 * 512);
            while let Some(ev) = e.drain_one() {
                out.push(ev);
            }
        }
        out.extend(e.flush());
        check_stream_invariants(&out, &scene, &cfg);
    }

    /// MAN-190 lifecycle: a pending hop is a self-contained value. Its
    /// source sample may be evicted from the delay line by a long
    /// no-drain pause (which also forfeits the centers the retention cap
    /// swallows -- by design, and excluded from this run's identity
    /// checks), yet the pending hop is still released with the amp and
    /// timestamp it was computed with, and the surviving centers stay
    /// ordered without duplicates.
    #[test]
    fn a_pending_hop_survives_eviction_of_its_source_sample() {
        let cfg = EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: 4.0,
            fallback_hops: 1_000_000,
            ..EvidenceConfig::default()
        };
        let scene: Vec<(f32, f32)> = (0..5000)
            .map(|i| (1.0 + 0.4 * ((i as f32) * 0.017).sin(), 0.2))
            .collect();
        let mut e = Evidence::new(cfg);
        let mut out = Vec::new();
        let mut paused_at = None;
        for (i, &(amp, noise)) in scene.iter().enumerate() {
            e.record(amp, noise, i as u64 * 512);
            if paused_at.is_none() {
                while let Some(ev) = e.drain_one() {
                    out.push(ev);
                    if ev.hop == 90 {
                        // Pause draining for the rest of the scene: the
                        // line fills to MAX_RETAIN and trims past sample
                        // 91 (the pending hop's own source sample).
                        paused_at = Some(i);
                        break;
                    }
                }
            }
        }
        assert!(paused_at.is_some(), "never paused at hop 90");
        while let Some(ev) = e.drain_one() {
            out.push(ev);
        }
        out.extend(e.flush());
        // The pending hop 91 was computed before the pause and its source
        // sample is gone; it must still come back with its original amp
        // and timestamp.
        let hop91 = out
            .iter()
            .find(|ev| ev.hop == 91)
            .expect("pending hop 91 released");
        assert!(
            (hop91.amp - scene[91].0).abs() < 1e-6,
            "pending hop's amp changed: {}",
            hop91.amp
        );
        assert_eq!(hop91.sample_ts, 91 * 512);
        // The centers the retention cap swallowed during the pause are
        // forfeited by design; everything released is ordered and unique.
        let ids: Vec<u64> = out.iter().map(|e| e.hop).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids, sorted, "hop ids ordered and unique");
    }

    #[test]
    fn shrinking_the_hold_window_drains_the_whole_backlog_in_one_call() {
        // Codex review, PR #161 round 9: a speed increase (`set_u_ref`
        // shrinking `h`) can make several already-buffered centers
        // eligible at once. The old one-in-one-out `push()` emitted only
        // the first of them per call, so the backlog took dozens of
        // subsequent hops to actually drain -- this proves it now drains
        // in the very next call instead.
        let mut e = Evidence::new(EvidenceConfig::default());
        let h_before = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops)
            .round() as usize;
        // Feed enough hops to reach steady state (well past h_before) so
        // the backlog is exactly at the initial window width.
        for i in 0..(h_before as u64 * 3) {
            e.push(1.0, 0.01, i);
        }
        e.set_u_ref(1.0); // shrinks h to hold_dits * 1.0 = 4
        let drained = e.push(1.0, 0.01, h_before as u64 * 3);
        assert!(
            drained.len() > 1,
            "expected the shrink to drain multiple backlogged centers in one call, got {}",
            drained.len()
        );
    }

    #[test]
    fn drain_one_reflects_a_set_u_ref_call_made_between_drains() {
        // Codex review, PR #161 round 11: a caller doing record() once
        // then drain_one() in a loop, calling set_u_ref BETWEEN drains,
        // must have each subsequent drain reflect the CURRENT h -- round
        // 9's original single-call batch design (materializing the whole
        // newly-eligible Vec inside one `push` call) could not do this,
        // since nothing inside that one call could ever update `h`
        // mid-loop.
        let mut e = Evidence::new(EvidenceConfig::default());
        let h_before = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops)
            .round() as usize;
        for i in 0..(h_before as u64 * 3) {
            e.push(1.0, 0.01, i);
        }
        e.record(1.0, 0.01, h_before as u64 * 3);
        let h_at_first_drain = e.h;
        assert!(
            e.drain_one().is_some(),
            "sanity: at least one center must be eligible before any shrink"
        );
        e.set_u_ref(1.0); // shrinks h to hold_dits * 1.0 = 4
        assert!(
            e.h < h_at_first_drain,
            "sanity: h must have actually shrunk"
        );
        assert!(
            e.drain_one().is_some(),
            "a set_u_ref call between two drain_one calls (no new record() in between) must \
             make another backlogged center immediately eligible under the new, smaller h"
        );
    }

    // [RULING, SDD execution 2026-09-09, Task 4 fix round 1] (I2): the
    // original `flush()` decremented `self.h` while draining, which shrank
    // BOTH the backward and forward window reach together even though only
    // the forward reach is genuinely limited by having no more input -- the
    // backward samples are still sitting in `line`. This corrupted `M` near
    // the end of every stream once the tail's own amplitude differed from
    // its noise floor (the other tests don't trigger it because their tails
    // happen to set `noise_amp` exactly equal to the tail amplitude).
    //
    // Scene: a solid mark, then a lower-but-still-gated-present tail (above
    // 2x noise, but well under half the held mark level) that ends the
    // stream. With correct (unshrunk) backward reach, `M` stays held near
    // the mark peak and the tail correctly reads as space-like (negative
    // llr); the old bug let `M` decay toward the tail's own amplitude as
    // the window shrank, flipping the sign to a phantom mark.
    //
    // Verified two ways: (1) directly, the tail must read negative under
    // the held mark; (2) by comparison against a second run padded with
    // `h` more hops of the same tail amplitude, so every hop in the
    // original run's range is reproduced by an ordinary `push()` with
    // genuine (untruncated) lookahead in the padded run -- flush()'s
    // output must match that ground truth hop-for-hop.
    #[test]
    fn flush_preserves_full_backward_reach_at_stream_end() {
        let warmup = 150usize;
        let tail = 40usize; // < h (60): full backward reach still spans back into the mark
        let noise_amp = 0.1f32;
        let h = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops)
            .round() as usize;

        let mut scene = vec![1.0f32; warmup];
        scene.extend(std::iter::repeat_n(0.3f32, tail));
        let flushed = run(&scene, noise_amp);
        assert_eq!(flushed.len(), warmup + tail);

        // Ground truth: pad with `h` more hops of the same tail amplitude so
        // every hop up to `warmup + tail` is emitted by ordinary push() with
        // real lookahead, never by flush(), in this second run.
        let mut padded_scene = scene.clone();
        padded_scene.extend(std::iter::repeat_n(0.3f32, h));
        let padded = run(&padded_scene, noise_amp);
        assert!(padded.len() >= warmup + tail);

        for i in 0..(warmup + tail) {
            let (f, p) = (flushed[i].llr, padded[i].llr);
            assert_eq!(
                f.signum(),
                p.signum(),
                "hop {i}: flush llr {f} vs continued-push (ground truth) llr {p} disagree in sign"
            );
            assert!(
                (f - p).abs() < 1e-3,
                "hop {i}: flush llr {f} vs continued-push (ground truth) llr {p} diverge"
            );
        }

        let last = flushed.last().unwrap();
        assert!(
            last.present,
            "tail should still be gated present given the held mark level"
        );
        assert!(
            last.llr < 0.0,
            "tail amplitude 0.3 under a held mark of 1.0 must read space-like, not a phantom mark: llr={}",
            last.llr
        );
    }

    // [Task 8a, SDD execution 2026-09-09] Reproduces Task 4 review's deferred
    // I3 finding: the old `push`/`flush` derived `center = line.len()-1-h`
    // from the CURRENT `self.h`, so a mid-stream `set_u_ref` (Task 8's HSMM
    // decoder is the first real caller) shifts which buffered sample is
    // treated as "center" for the next emission -- duplicating an
    // already-emitted sample, skipping one entirely, or corrupting the
    // `prefix` running sum. This test feeds a varying-amplitude stream
    // through two mid-stream `set_u_ref` calls (h shrinks, then grows far
    // past its original value) and checks the emitted stream is a clean,
    // order-preserving, one-to-one image of the input regardless.
    #[test]
    fn set_u_ref_mid_stream_preserves_emission_order_and_prefix() {
        let mut e = Evidence::new(EvidenceConfig::default());
        e.set_u_ref(20.0); // h = 4*20 = 80
        let n_hops = 300usize;
        let mut fed_amps = Vec::with_capacity(n_hops);
        let mut out = Vec::with_capacity(n_hops);
        for i in 0..n_hops {
            // Varying but always well above the noise floor, so the keying
            // gate stays present throughout -- this test is about ordering,
            // not the gate/LLR math (already covered elsewhere).
            let amp = 0.6 + 0.3 * ((i as f32) * 0.037).sin();
            fed_amps.push(amp);
            if i == 100 {
                e.set_u_ref(10.0); // h shrinks to 40
            }
            if i == 200 {
                e.set_u_ref(40.0); // h grows to 160, past its original value
            }
            out.extend(e.push(amp, 0.05, i as u64));
        }
        out.extend(e.flush());

        // Every fed hop must be emitted exactly once: no duplicate, no gap.
        assert_eq!(
            out.len(),
            n_hops,
            "expected exactly one emission per pushed hop"
        );

        // (a) hop indices are exactly 0..N-1 with no duplicate or gap.
        let hops: Vec<u64> = out.iter().map(|h| h.hop).collect();
        let expected: Vec<u64> = (0..out.len() as u64).collect();
        assert_eq!(
            hops, expected,
            "hop sequence must be contiguous with no dup/gap"
        );

        // (b) amp values, read in order, exactly match what was fed -- this
        // is what actually catches a duplicated/skipped center sample: the
        // old code kept the hop counter contiguous even while silently
        // rebinding it to the wrong buffered sample.
        for (i, h) in out.iter().enumerate() {
            assert!(
                (h.amp - fed_amps[i]).abs() < 1e-6,
                "hop {i}: emitted amp {} != fed amp {} (sample duplicated or skipped)",
                h.amp,
                fed_amps[i]
            );
        }

        // (c) prefix is a clean running sum of llr in emission order -- no
        // out-of-order or duplicate emission folded an extra/missing term
        // into it.
        for i in 1..out.len() {
            let delta = out[i].prefix - out[i - 1].prefix;
            assert!(
                (delta - out[i].llr as f64).abs() < 1e-6,
                "hop {i}: prefix delta {delta} != llr {} (prefix corrupted)",
                out[i].llr
            );
        }
    }

    /// MAN-193: total centered-maximum work at a fixed `h` is linear in the
    /// input and emitted hops. Every remaining maximum path (actual query
    /// and sign-edge lookahead) counts its smoothed-amplitude reads.
    fn assert_linear_max_work(e: &Evidence, inputs: u64, emitted: u64, ctx: &str) {
        let reads = e.max_reads.get();
        assert!(
            reads <= inputs + emitted,
            "{ctx}: {reads} maximum reads for {inputs} inputs and {emitted} emitted hops"
        );
    }

    #[test]
    fn centered_max_work_is_linear_for_300_tracks_at_h_224() {
        // The ticket's scenario: 300 interleaved tracks at the default
        // slow-speed limit (hold_dits 4 x u_max 56 = 224), 3,750 input hops
        // each (10 s) and a full flush. A setter that rounds to the same h
        // runs after every hop, as `push_hop_hsmm`'s feedback does.
        let cfg = EvidenceConfig {
            u_init_hops: 56.0,
            ..EvidenceConfig::default()
        };
        let hops = 3_750u64;
        let mut tracks: Vec<Evidence> = (0..300).map(|_| Evidence::new(cfg.clone())).collect();
        let mut emitted = vec![0u64; tracks.len()];
        for i in 0..hops {
            for (t, e) in tracks.iter_mut().enumerate() {
                let amp = 0.8 + 0.2 * (((i + t as u64) % 19) as f32 / 19.0);
                e.record(amp, 0.1, i);
                while e.drain_one().is_some() {
                    emitted[t] += 1;
                    e.set_u_ref(if i % 2 == 0 { 56.0 } else { 56.05 });
                    assert_eq!(e.h, 224);
                }
            }
        }
        for (t, e) in tracks.iter_mut().enumerate() {
            while e.flush_one().is_some() {
                emitted[t] += 1;
            }
            assert_eq!(emitted[t], hops, "track {t}");
            assert_linear_max_work(e, hops, emitted[t], &format!("track {t}"));
            assert_eq!(
                e.max_cache.ops.builds, 1,
                "track {t}: a fixed window rebuilt"
            );
        }
    }

    #[test]
    fn centered_max_work_stays_linear_across_retention_trimming() {
        let hops = 3 * MAX_RETAIN as u64 + 500;
        let mut e = Evidence::new(EvidenceConfig {
            u_init_hops: 56.0,
            ..EvidenceConfig::default()
        });
        let mut emitted = 0u64;
        for i in 0..hops {
            emitted += e
                .push(0.5 + 0.4 * ((i as f32) * 0.013).sin(), 0.05, i)
                .len() as u64;
        }
        emitted += e.flush().len() as u64;
        assert_eq!(emitted, hops);
        assert_linear_max_work(&e, hops, emitted, "single long track");
    }

    /// MAN-193 differential oracle: the same calls made on a cached
    /// `Evidence` and a full-scan reference, with every call's result
    /// compared bit for bit, `None` included.
    struct Twin {
        fast: Evidence,
        slow: Evidence,
        out: Vec<HopEvidence>,
    }

    fn assert_same(fast: &Option<HopEvidence>, slow: &Option<HopEvidence>, ctx: &str) {
        match (fast, slow) {
            (None, None) => {}
            (Some(f), Some(s)) => {
                assert_eq!(f.hop, s.hop, "{ctx}: hop");
                assert_eq!(f.sample_ts, s.sample_ts, "{ctx}: sample_ts");
                assert_eq!(f.amp.to_bits(), s.amp.to_bits(), "{ctx}: amp");
                assert_eq!(
                    f.noise_level.to_bits(),
                    s.noise_level.to_bits(),
                    "{ctx}: noise"
                );
                assert_eq!(
                    f.mark_level.to_bits(),
                    s.mark_level.to_bits(),
                    "{ctx}: hop {} mark_level {} vs reference {}",
                    f.hop,
                    f.mark_level,
                    s.mark_level
                );
                assert_eq!(f.llr.to_bits(), s.llr.to_bits(), "{ctx}: hop {} llr", f.hop);
                assert_eq!(f.present, s.present, "{ctx}: hop {} present", f.hop);
                assert_eq!(f.anchor, s.anchor, "{ctx}: hop {} anchor", f.hop);
                assert_eq!(
                    f.prefix.to_bits(),
                    s.prefix.to_bits(),
                    "{ctx}: hop {} prefix",
                    f.hop
                );
            }
            _ => panic!("{ctx}: readiness differs: cached {fast:?} vs reference {slow:?}"),
        }
    }

    impl Twin {
        fn new(cfg: EvidenceConfig) -> Self {
            Twin {
                fast: Evidence::new(cfg.clone()),
                slow: Evidence::reference(cfg),
                out: Vec::new(),
            }
        }
        fn record(&mut self, amp: f32, noise: f32) {
            let ts = self.fast.hop_in * 512;
            self.fast.record(amp, noise, ts);
            self.slow.record(amp, noise, ts);
        }
        fn set_u(&mut self, u: f32) {
            self.fast.set_u_ref(u);
            self.slow.set_u_ref(u);
            assert_eq!(self.fast.h, self.slow.h);
        }
        fn take(&mut self, f: Option<HopEvidence>, s: Option<HopEvidence>, ctx: &str) -> bool {
            assert_same(&f, &s, ctx);
            match f {
                Some(ev) => {
                    self.out.push(ev);
                    true
                }
                None => false,
            }
        }
        fn drain(&mut self) -> bool {
            let (f, s) = (self.fast.drain_one(), self.slow.drain_one());
            self.take(f, s, "drain_one")
        }
        fn flush(&mut self) -> bool {
            let (f, s) = (self.fast.flush_one(), self.slow.flush_one());
            self.take(f, s, "flush_one")
        }
        fn drain_all(&mut self) {
            while self.drain() {}
        }
        /// Flush to exhaustion, then once more: exhaustion must stick.
        fn flush_all(&mut self) {
            while self.flush() {}
            assert!(!self.flush(), "flush_one emitted after exhaustion");
        }
        fn builds(&self) -> u64 {
            self.fast.max_cache.ops.builds
        }
    }

    fn cfg_h(h: usize) -> EvidenceConfig {
        EvidenceConfig {
            hold_dits: 1.0,
            u_init_hops: h as f32,
            ..EvidenceConfig::default()
        }
    }

    fn seeded_amps(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed.max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                0.05 + (x >> 40) as f32 / (1u64 << 24) as f32
            })
            .collect()
    }

    /// Streams paired with their noise amplitude per hop.
    fn oracle_streams(n: usize) -> Vec<(&'static str, Vec<f32>, Vec<f32>)> {
        let flat = |v: f32| vec![v; n];
        let mut keyed_env = keyed(13, 40.0, n / 104 + 1);
        keyed_env.truncate(n);
        vec![
            ("keyed", keyed_env, flat(0.005)),
            (
                "ramp",
                (0..n).map(|i| i as f32 / n as f32).collect(),
                flat(0.01),
            ),
            (
                "impulse",
                (0..n)
                    .map(|i| if i % 700 == 350 { 4.0 } else { 0.1 })
                    .collect(),
                flat(0.02),
            ),
            (
                "repeated-peak",
                (0..n).map(|i| if i % 50 < 2 { 1.0 } else { 0.3 }).collect(),
                flat(0.05),
            ),
            ("silence", flat(0.0), flat(0.0)),
            (
                "changing-noise",
                (0..n)
                    .map(|i| 0.6 + 0.3 * ((i as f32) * 0.02).sin())
                    .collect(),
                (0..n)
                    .map(|i| 0.05 + 0.25 * ((i as f32) * 0.003).sin().abs())
                    .collect(),
            ),
            (
                "seeded",
                seeded_amps(n, 193),
                seeded_amps(n, 7).iter().map(|v| v * 0.2).collect(),
            ),
        ]
    }

    #[test]
    fn cached_maximum_matches_the_scan_at_fixed_widths() {
        let n = 3000;
        for h in [1usize, 60, 224, 1024] {
            for (name, amps, noise) in oracle_streams(n) {
                let mut t = Twin::new(cfg_h(h));
                for i in 0..n {
                    t.record(amps[i], noise[i]);
                    t.drain_all();
                }
                t.flush_all();
                assert_eq!(t.out.len(), n, "{name} h={h}");
                assert_eq!(t.builds(), 1, "{name} h={h}: a fixed window rebuilt");
            }
        }
    }

    #[test]
    fn setters_that_round_to_the_same_h_never_rebuild() {
        let n = 2000;
        let amps = seeded_amps(n, 21);
        let mut t = Twin::new(EvidenceConfig::default());
        for (i, &a) in amps.iter().enumerate() {
            t.record(a, 0.02);
            while t.drain() {
                // 4 x {15.0, 15.04, 14.96} all round to h = 60.
                t.set_u([15.0, 15.04, 14.96][i % 3]);
            }
        }
        t.flush_all();
        assert_eq!(t.out.len(), n);
        assert_eq!(t.builds(), 1, "same-rounded setters must not rebuild");
    }

    #[test]
    fn resizes_between_individual_drains_match_the_scan() {
        // A peak 30 hops ahead of the drained centers: h = 40 sees it,
        // h = 4 does not.
        let mut t = Twin::new(cfg_h(40));
        let mut amps = vec![0.3f32; 600];
        for a in &mut amps[300..305] {
            *a = 3.0;
        }
        for &a in &amps[..330] {
            t.record(a, 0.01);
        }
        // Catch-up batch at h = 40: centers up to 289 see the peak.
        t.drain_all();
        let wide = t.out.last().unwrap().mark_level;
        assert!(
            wide > 2.0,
            "sanity: the wide window holds the peak ({wide})"
        );
        // Shrink between drains with no intervening record: a backlog of
        // centers becomes eligible and the peak leaves their windows.
        t.set_u(4.0);
        let before = t.out.len();
        for _ in 0..5 {
            assert!(t.drain(), "a shrink must make backlogged centers eligible");
        }
        let narrow = t.out.last().unwrap().mark_level;
        assert!(
            narrow < 1.0,
            "the shrunk window must exclude the peak ({narrow})"
        );
        t.drain_all();
        assert!(t.out.len() > before + 5);
        // Growth pauses eligibility until enough lookahead is recorded.
        t.set_u(200.0);
        assert!(!t.drain(), "growth must pause eligibility");
        for &a in &amps[330..] {
            t.record(a, 0.01);
            t.drain_all();
            t.set_u(if t.out.len().is_multiple_of(2) {
                40.0
            } else {
                4.0
            });
        }
        t.flush_all();
        assert_eq!(t.out.len(), amps.len());
        // Center 326, the first computed after the growth, is centered at
        // h = 40 again and must hold the peak the h = 4 centers excluded.
        assert!(
            t.out[326..].iter().any(|h| h.mark_level > 2.0),
            "the peak must be restored in a regrown window"
        );
    }

    #[test]
    fn resizes_between_individual_flushes_match_the_scan() {
        // Peak fixture for EOF: a peak 100 hops before the end, flushed
        // with h oscillating 223/225 and with large 4 <-> 224 swings.
        for swing in [[223.0f32, 225.0], [4.0, 224.0]] {
            let mut t = Twin::new(cfg_h(224));
            let n = 1200;
            for i in 0..n {
                t.record(if i == n - 100 { 6.0 } else { 0.4 }, 0.01);
                t.drain_all();
            }
            let mut levels = Vec::new();
            loop {
                t.set_u(swing[t.out.len() % 2]);
                if !t.flush() {
                    break;
                }
                levels.push(t.out.last().unwrap().mark_level);
            }
            assert!(!t.flush(), "flush_one emitted after exhaustion");
            assert_eq!(t.out.len(), n);
            assert!(t.builds() > 1, "{swing:?}: resizes must rebuild");
            if swing[0] == 4.0 {
                // The large swing must actually exclude and then restore the
                // peak for the same region of centers.
                let held = levels.iter().filter(|&&m| m > 1.0).count();
                let lost = levels.iter().filter(|&&m| m < 1.0).count();
                assert!(
                    held > 0 && lost > 0,
                    "swing must change M: {held} held, {lost} lost"
                );
                assert!(
                    levels
                        .windows(3)
                        .any(|w| w[0] > 1.0 && w[1] < 1.0 && w[2] > 1.0),
                    "a regrown window must restore the excluded peak"
                );
            }
        }
    }

    #[test]
    fn short_and_empty_inputs_match_the_scan() {
        // Empty input, then one sample, two samples and fewer than h.
        for len in [0usize, 1, 2, 30, 59, 60, 61] {
            let mut t = Twin::new(EvidenceConfig::default());
            for i in 0..len {
                t.record(0.2 + 0.1 * (i % 3) as f32, 0.01);
                t.drain_all();
            }
            t.flush_all();
            assert!(!t.flush(), "repeated flush after exhaustion");
            assert_eq!(t.out.len(), len);
        }
    }

    #[test]
    fn long_streams_with_resizes_near_retention_rollover_match_the_scan() {
        let n = 3 * MAX_RETAIN + 600;
        let amps = seeded_amps(n, 99);
        let mut t = Twin::new(EvidenceConfig {
            u_init_hops: 56.0,
            ..EvidenceConfig::default()
        });
        for (i, &a) in amps.iter().enumerate() {
            if [MAX_RETAIN - 3, 2 * MAX_RETAIN + 1, 3 * MAX_RETAIN].contains(&i) {
                t.set_u(15.0);
            }
            if [MAX_RETAIN + 40, 2 * MAX_RETAIN + 50].contains(&i) {
                t.set_u(56.0);
            }
            t.record(a, 0.02);
            t.drain_all();
        }
        t.flush_all();
        assert_eq!(t.out.len(), n);
        for (i, ev) in t.out.iter().enumerate() {
            assert_eq!(ev.hop, i as u64);
            assert_eq!(ev.sample_ts, i as u64 * 512, "sample identity at hop {i}");
            assert_eq!(
                ev.amp.to_bits(),
                amps[i].to_bits(),
                "sample identity at hop {i}"
            );
        }
    }

    #[test]
    fn a_right_edge_peak_recorded_after_the_lookahead_enters_the_next_center() {
        let h = 4usize;
        let mut t = Twin::new(cfg_h(h));
        for _ in 0..20 {
            t.record(0.5, 0.01);
            t.drain_all();
        }
        // Center 15 was just computed (its window reaching only sample 19)
        // and is held pending until its successor is computed (MAN-190);
        // sample 20 arrives afterwards.
        let c = t.out.len();
        assert_eq!(c, 20 - h - 1);
        t.record(5.0, 0.01);
        let a_s = t.fast.line.back().unwrap().1;
        assert!(t.drain()); // computes center 16, releases 15
        t.record(0.5, 0.01);
        assert!(t.drain()); // computes center 17, releases 16
        let next = t.out[c + 1];
        assert_eq!(next.hop, (c + 1) as u64);
        assert!(
            next.mark_level > t.out[c].mark_level,
            "the actual maximum must include the newly recorded peak"
        );
        assert_eq!(next.mark_level.to_bits(), a_s.to_bits());
    }

    #[test]
    fn the_benchmark_resize_stress_schedule_rebuilds_about_every_third_hop() {
        // `benches/evidence_300_tracks.rs`'s stress case: h switches between
        // 223 and 225 every third pushed hop. Growth to 225 retreats the
        // next window's left bound and the shrink to 223 its right bound,
        // so each switch costs one rebuild of about 2h+1 samples.
        let hops = 3_750u64;
        let mut t = Twin::new(EvidenceConfig {
            u_init_hops: 56.0,
            ..EvidenceConfig::default()
        });
        for i in 0..hops {
            t.record(0.5 + 0.4 * ((i as f32) * 0.07).sin(), 0.02);
            t.drain_all();
            t.set_u([55.75, 56.25][(i as usize / 3) % 2]);
        }
        t.flush_all();
        assert_eq!(t.out.len() as u64, hops);
        let ops = &t.fast.max_cache.ops;
        // About one rebuild per three emitted hops (a few switches land
        // while a grown window is still waiting for lookahead).
        assert!(
            ops.builds * 4 > hops,
            "{} builds for {hops} hops: the stress case is not stressing",
            ops.builds
        );
        assert!(ops.rebuilt_admissions >= (ops.builds - 1) * 400);
        println!(
            "stress schedule: {} builds, {} rebuilt admissions, {} admissions for {hops} hops",
            ops.builds, ops.rebuilt_admissions, ops.admitted
        );
    }
}
