//! Per-track narrowband refinement: an optional, per-hop channel derotator
//! and lowpass that sharpens a track's amplitude estimate using its
//! fractional centroid offset. SPEC v2 §3. Enabled when `decode.refine_bw_hz`
//! is greater than zero (default 0, meaning off); the engine call site is
//! out of scope for this module (deferred to MAN-168).

use num_complex::Complex32;

/// SPEC v2 §3: fixed hop rate of the channelizer (`fo`).
const HOP_RATE_HZ: f64 = 375.0;
/// SPEC v2 §3: per-channel spacing (`Δ`), matching `channelizer.rs`'s
/// `CHANNEL_SPACING_HZ`.
const CHANNEL_SPACING_HZ: f64 = 93.75;
/// SPEC v2 §3: 11-tap FIR.
const TAPS: usize = 11;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Design the 11-tap Hamming-windowed sinc lowpass with two-sided
/// bandwidth `bw_hz` (cutoff `fc = bw_hz / 2`, matching `proto.rs`'s
/// `f_c = Δ/2` convention), sample rate `fo = 375 Hz` (SPEC v2 §3).
/// Designed once at startup in `f64`, stored `f32`, normalized for unity
/// DC gain.
fn design_refine_fir(bw_hz: f32) -> [f32; TAPS] {
    let len = TAPS;
    let center = (len - 1) as f64 / 2.0;
    let fc = bw_hz as f64 / 2.0;
    let mut h = [0.0f64; TAPS];
    let mut sum = 0.0f64;
    for (i, tap) in h.iter_mut().enumerate() {
        let x = i as f64 - center;
        // Hamming window: w[i] = 0.54 - 0.46*cos(2*pi*i/(L-1)).
        let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (len - 1) as f64).cos();
        *tap = sinc(2.0 * fc / HOP_RATE_HZ * x) * w;
        sum += *tap;
    }
    // Unity DC gain.
    let mut out = [0.0f32; TAPS];
    for (o, &v) in out.iter_mut().zip(h.iter()) {
        *o = (v / sum) as f32;
    }
    out
}

/// Streaming per-track narrowband refiner (SPEC v2 §3): derotates the
/// owned channel's samples by the fractional centroid offset `delta`
/// (channels), lowpass-filters at `bw_hz` to reject neighboring-channel
/// energy, and returns the filtered magnitude `a[t] = |z[t]|`.
pub struct Refiner {
    taps: [f32; TAPS],
    delta: f32,
    /// f64 phase accumulator (SPEC v2 §3, determinism), wrapped each hop.
    phase: f64,
    /// Ring of the last TAPS derotated samples `y[t]`, oldest first.
    hist: [Complex32; TAPS],
}

impl Refiner {
    /// `bw_hz`: two-sided lowpass bandwidth (Hz, `fc = bw_hz / 2`) for the
    /// 11-tap FIR -- callers must not construct a `Refiner` when
    /// `decode.refine_bw_hz` is 0 (disabled); `bw_hz = 0.0` here would
    /// silently design a working (if very narrow) filter, not a bypass.
    /// `delta`: initial fractional centroid offset, in channels.
    pub fn new(bw_hz: f32, delta: f32) -> Self {
        debug_assert!(
            bw_hz > 0.0,
            "Refiner::new called with bw_hz <= 0.0; the caller must check \
             decode.refine_bw_hz > 0 before constructing a Refiner at all"
        );
        Refiner {
            taps: design_refine_fir(bw_hz),
            delta,
            phase: 0.0,
            hist: [Complex32::new(0.0, 0.0); TAPS],
        }
    }

    /// Update the centroid offset. Per SPEC v2 §3, this resets nothing —
    /// the phase accumulator and FIR history both carry over.
    pub fn set_delta(&mut self, delta: f32) {
        self.delta = delta;
    }

    /// Reset the FIR history (and phase accumulator) — call when the
    /// owned channel `c` changes (SPEC v2 §3: "a change of c resets the
    /// FIR history"). Channel-change detection itself lives at the
    /// engine call site (MAN-168), out of scope here.
    pub fn reset(&mut self) {
        self.hist = [Complex32::new(0.0, 0.0); TAPS];
        self.phase = 0.0;
    }

    /// Feed one channel sample `X[c, t]`; returns `a[t] = |z[t]|`.
    pub fn push(&mut self, sample: Complex32) -> f32 {
        // Step 1: derotate by the running f64 phase accumulator.
        let y = sample * Complex32::from_polar(1.0, -self.phase as f32);

        // Shift the history ring: drop oldest, append y[t] as newest.
        for i in 0..TAPS - 1 {
            self.hist[i] = self.hist[i + 1];
        }
        self.hist[TAPS - 1] = y;

        // Step 2: z[t] = sum_i g[i] * y[t-i]. hist[TAPS-1] is y[t]
        // (i=0), hist[0] is y[t-(TAPS-1)] (i=TAPS-1) -- oldest-first
        // ring paired with taps in reverse.
        let mut acc_re = 0.0f64;
        let mut acc_im = 0.0f64;
        for i in 0..TAPS {
            let g = self.taps[i] as f64;
            let yv = self.hist[TAPS - 1 - i];
            acc_re += g * yv.re as f64;
            acc_im += g * yv.im as f64;
        }
        let z = Complex32::new(acc_re as f32, acc_im as f32);

        // Advance the phase accumulator for next sample, wrapped each hop.
        let step =
            2.0 * std::f64::consts::PI * self.delta as f64 * CHANNEL_SPACING_HZ / HOP_RATE_HZ;
        self.phase = (self.phase + step).rem_euclid(2.0 * std::f64::consts::PI);

        // Step 3: a[t] = |z[t]|.
        z.norm()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refiner_passes_a_centered_tone_and_rejects_a_neighbor_120_hz_away() {
        // NOTE: the brief's original test fed both signals through ONE shared
        // `Refiner`, alternating `r.push(tone)` / `r.push(nb)` on every
        // iteration. That is provably broken: with strict 1:1 alternation,
        // any 11-tap window of the combined raw stream contains ~5-6 samples
        // from each subsequence, so the filtered output computed right after
        // a `tone` push and right after the following `nb` push are built
        // from nearly the same 11-sample window (offset by one element) --
        // they cannot differ by the "reject 120 Hz by >=20 dB" margin the
        // assertion demands, regardless of how good the lowpass is. Measured
        // empirically (scratch run) with this crate's actual FIR: pass 1469
        // rej 1438, ratio ~1.02, not >10 -- confirms this is a test-authoring
        // bug (shared filter state across two unrelated signals), not an
        // implementation bug. Fixed by giving each signal its own `Refiner`
        // (independent phase accumulator + FIR history), which is what
        // "passes a centered tone and rejects a neighbor" actually needs to
        // test: two clean single-tone runs through the frequency response,
        // not one filter's response to a two-tone interleaved stream.
        let mut r_pass = Refiner::new(30.0, 0.0); // bw, delta channels
        let mut r_rej = Refiner::new(30.0, 0.0);
        let fo = 375.0;
        let mut pass = 0.0f32;
        let mut rej = 0.0f32;
        for t in 0..3000 {
            let tt = t as f64 / fo;
            let tone = Complex32::new(1.0, 0.0); // DC = channel center
            let nb = Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 120.0 * tt) as f32);
            let a = r_pass.push(tone);
            let b = r_rej.push(nb);
            if t > 100 {
                pass += a;
                rej += b;
            }
        }
        assert!(pass / rej > 10.0, "pass {pass} rej {rej}"); // >= 20 dB
    }

    #[test]
    fn refiner_rotates_by_centroid_offset() {
        // A tone at +0.3 channel (+28 Hz) with delta = 0.3 must come out like DC.
        let mut r = Refiner::new(30.0, 0.3);
        let fo = 375.0;
        let mut acc = 0.0f32;
        for t in 0..3000 {
            let tt = t as f64 / fo;
            let x =
                Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 0.3 * 93.75 * tt) as f32);
            let a = r.push(x);
            if t > 100 {
                acc += a;
            }
        }
        assert!(acc / 2900.0 > 0.9, "mean amplitude {}", acc / 2900.0);
    }
}
