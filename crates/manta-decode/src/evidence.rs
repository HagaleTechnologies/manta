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
        EvidenceConfig { sigma_u: 0.30, llr_clip: 10.0, hold_dits: 4.0, fallback_hops: 8, tau_a_ms: 8.0, u_init_hops: 15.0 }
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
    last_sign: i8,
    last_present: bool,
}

impl Evidence {
    pub fn new(cfg: EvidenceConfig) -> Self {
        let alpha_a = (1.0 - (-HOP_MS / cfg.tau_a_ms).exp()) as f32;
        let h = (cfg.hold_dits * cfg.u_init_hops).round().max(1.0) as usize;
        Evidence { cfg, alpha_a, a_s: None, h, line: VecDeque::new(), hop_in: 0, hop_out: 0, prefix: 0.0, last_sign: 0, last_present: false }
    }

    /// SPEC v2 §1.2: h = round(hold_dits * u_ref). Takes effect for the next
    /// emitted hop; the delay line simply grows or shrinks by the difference.
    pub fn set_u_ref(&mut self, u_hops: f32) {
        self.h = (self.cfg.hold_dits * u_hops).round().max(1.0) as usize;
    }

    pub fn push(&mut self, amp: f32, noise_amp: f32, sample_ts: u64) -> Option<HopEvidence> {
        let a_s = match self.a_s { None => amp, Some(p) => p + self.alpha_a * (amp - p) };
        self.a_s = Some(a_s);
        self.line.push_back((amp, a_s, noise_amp, sample_ts));
        self.hop_in += 1;
        // Emit once we have h hops of look-ahead past the center element.
        if self.line.len() > 2 * self.h + 1 { self.line.pop_front(); }
        if self.line.len() < self.h + 1 { return None; }
        let center = self.line.len() - 1 - self.h;
        Some(self.emit(center))
    }

    pub fn flush(&mut self) -> Vec<HopEvidence> {
        // SDD execution 2026-09-09, Task 4 fix round 1: no new input arrives
        // during flush, so every already-buffered element keeps its full
        // backward reach at the current `h` -- only the forward reach is
        // genuinely limited by `line.len()-1`, which `emit()`'s existing
        // `.min()` already handles correctly. The original version mutated
        // `self.h` here, shrinking the backward reach too and corrupting M
        // near the end of every stream.
        let mut out = Vec::new();
        let next_center = if self.line.len() >= self.h + 1 {
            self.line.len() - self.h
        } else {
            0
        };
        for center in next_center..self.line.len() {
            out.push(self.emit(center));
        }
        out
    }

    fn emit(&mut self, center: usize) -> HopEvidence {
        let (amp, _, n, ts) = self.line[center];
        // Centered maximum of the smoothed amplitude over [center-h, center+h]
        // (linear scan; window <= 2*hold_dits*u_ref+1, deterministic).
        let lo = center.saturating_sub(self.h);
        let hi = (center + self.h).min(self.line.len() - 1);
        let mut m = 0.0f32;
        for i in lo..=hi { m = m.max(self.line[i].1); }
        let present = m >= 2.0 * n;
        let llr = if present {
            let u = ((amp - n) / (m - n).max(1e-9)).clamp(-0.5, 1.5);
            ((u - 0.5) / (self.cfg.sigma_u * self.cfg.sigma_u)).clamp(-self.cfg.llr_clip, self.cfg.llr_clip)
        } else {
            -self.cfg.llr_clip
        };
        let sign: i8 = if llr > 0.0 { 1 } else if llr < 0.0 { -1 } else { 0 };

        // Edge trigger (SPEC v2 §1.7): a hop is an anchor at a half-amplitude
        // crossing. The crossing sample itself is ambiguous between "last hop
        // of the old sign" and "first hop of the new sign" (the emulated
        // channel ramp puts the true crossing exactly between two discrete
        // hops); tagging the LATER hop pushes every measured edge one hop
        // past the true keying edge system-atically. Since the delay line
        // already buffers `h` hops of lookahead (h >> 1), we can look one
        // hop past `center` — already resident in the buffer, no extra
        // latency — and anchor the EARLIER hop of a transition instead,
        // which keeps edges anchored to within one hop of the true edge at
        // any keying depth. Fall back to the backward-looking (previous
        // emitted hop) comparison only at the very end of the stream, where
        // no lookahead sample exists (final `flush` hops).
        let sign_edge = if !present {
            // llr is pinned at -L for the whole gated-off span; there is no
            // internal sign to cross. The gate transition itself is caught
            // separately by the `present != self.last_present` check below.
            false
        } else {
            match self.line.get(center + 1) {
                Some(&(next_amp, _, _, _)) => {
                    let u = ((next_amp - n) / (m - n).max(1e-9)).clamp(-0.5, 1.5);
                    let next_llr = ((u - 0.5) / (self.cfg.sigma_u * self.cfg.sigma_u))
                        .clamp(-self.cfg.llr_clip, self.cfg.llr_clip);
                    let next_sign: i8 = if next_llr > 0.0 { 1 } else if next_llr < 0.0 { -1 } else { 0 };
                    next_sign != sign
                }
                None => sign != self.last_sign,
            }
        };
        let anchor = sign_edge
            || self.hop_out % self.cfg.fallback_hops as u64 == 0
            || present != self.last_present;
        self.last_sign = sign;
        self.last_present = present;
        self.prefix += llr as f64;
        let ev = HopEvidence { hop: self.hop_out, sample_ts: ts, llr, present, anchor, prefix: self.prefix, amp, mark_level: m, noise_level: n };
        self.hop_out += 1;
        ev
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
        let seg = |v: &mut Vec<f32>, on: bool, n: usize| for _ in 0..n { v.push(if on { 1.0 } else { lo }); };
        for _ in 0..reps { seg(&mut v, true, dit_hops); seg(&mut v, false, dit_hops); seg(&mut v, true, 3 * dit_hops); seg(&mut v, false, 3 * dit_hops); }
        // 4-hop ramps
        let mut out = v.clone();
        for i in 1..v.len() { if (v[i] - v[i-1]).abs() > 0.1 { for j in 0..4 { let k = i + j; if k < out.len() { let f = (j as f32 + 0.5) / 4.0; out[k] = v[i-1] + f * (v[i] - v[i-1]); } } } }
        out
    }

    fn run(env: &[f32], noise_amp: f32) -> Vec<HopEvidence> {
        run_with_config(env, noise_amp, EvidenceConfig::default())
    }

    fn run_with_config(env: &[f32], noise_amp: f32, cfg: EvidenceConfig) -> Vec<HopEvidence> {
        let mut e = Evidence::new(cfg);
        let mut out = Vec::new();
        for (i, &a) in env.iter().enumerate() { if let Some(h) = e.push(a, noise_amp, i as u64 * 512) { out.push(h); } }
        out.extend(e.flush());
        out
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
        for _ in 0..300 { if let Some(h) = e.push(1.0, 0.0, 0) { out.push(h); } }
        if let Some(h) = e.push(0.75, 0.0, 0) { out.push(h); }
        if let Some(h) = e.push(0.25, 0.0, 0) { out.push(h); }
        for _ in 0..200 { if let Some(h) = e.push(0.75, 0.0, 0) { out.push(h); } }
        out.extend(e.flush());
        let hi = out.iter().find(|h| (h.amp - 0.75).abs() < 1e-6 && h.mark_level > 0.99).unwrap().llr;
        let lo = out.iter().find(|h| (h.amp - 0.25).abs() < 1e-6 && h.mark_level > 0.99).unwrap().llr;
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
            let cfg = EvidenceConfig { fallback_hops: 1_000_000, ..EvidenceConfig::default() };
            let hops = run_with_config(&env, 10f32.powf(-depth / 20.0), cfg);
            let anchors: Vec<u64> = hops.iter().filter(|h| h.anchor && h.llr.signum() != 0.0).map(|h| h.hop).collect();
            // true edges of the "dit gap dah gap" pattern (period 8 dits) start at 0 in the un-ramped envelope;
            // each measured edge must be within 1 hop of a true edge at 13k or 13k+... boundaries.
            let mut off_by = 0;
            for &a in &anchors {
                let m = a % (8 * 13);
                let nearest = [0u64, 13, 26, 65].iter().map(|&t| (m as i64 - t as i64).abs().min(104 - (m as i64 - t as i64).abs())).min().unwrap();
                if nearest > 1 { off_by += 1; }
            }
            assert!(off_by * 20 < anchors.len(), "depth {depth}: {off_by}/{} anchors more than 1 hop from a true edge", anchors.len());
        }
    }

    #[test]
    fn segment_sums_measure_true_durations_at_60_db() {
        let env = keyed(13, 60.0, 10);
        let hops = run(&env, 0.001);
        // Count consecutive positive-llr runs after warm-up: dits must be 13±1, dahs 39±1.
        let mut runs = Vec::new(); let mut cur = 0; let mut sign = false;
        for h in hops.iter().skip(200) { let s = h.llr > 0.0; if s == sign { cur += 1; } else { if sign { runs.push(cur); } cur = 1; sign = s; } }
        let dits = runs.iter().filter(|&&r| (12..=14).contains(&r)).count();
        let dahs = runs.iter().filter(|&&r| (38..=40).contains(&r)).count();
        assert!(dits >= 8 && dahs >= 8, "runs {runs:?}");
    }

    #[test]
    fn gate_off_when_keying_depth_under_6_db() {
        let mut e = Evidence::new(EvidenceConfig::default());
        let last = (0..600).filter_map(|_| e.push(1.0, 0.8, 0)).last().unwrap();
        assert!(!last.present);
        assert_eq!(last.llr, -EvidenceConfig::default().llr_clip);
    }

    #[test]
    fn delay_equals_hold_window() {
        let mut e = Evidence::new(EvidenceConfig::default());
        let h = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops).round() as usize;
        for i in 0..h { assert!(e.push(1.0, 0.01, i as u64).is_none(), "hop {i} emitted early"); }
        assert!(e.push(1.0, 0.01, h as u64).is_some());
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
        let h = (EvidenceConfig::default().hold_dits * EvidenceConfig::default().u_init_hops).round() as usize;

        let mut scene = vec![1.0f32; warmup];
        scene.extend(std::iter::repeat(0.3f32).take(tail));
        let flushed = run(&scene, noise_amp);
        assert_eq!(flushed.len(), warmup + tail);

        // Ground truth: pad with `h` more hops of the same tail amplitude so
        // every hop up to `warmup + tail` is emitted by ordinary push() with
        // real lookahead, never by flush(), in this second run.
        let mut padded_scene = scene.clone();
        padded_scene.extend(std::iter::repeat(0.3f32).take(h));
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
        assert!(last.present, "tail should still be gated present given the held mark level");
        assert!(
            last.llr < 0.0,
            "tail amplitude 0.3 under a held mark of 1.0 must read space-like, not a phantom mark: llr={}",
            last.llr
        );
    }
}
