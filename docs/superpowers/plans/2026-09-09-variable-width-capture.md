# Variable-Width Capture (Decimated Channelizer Input) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let manta capture narrower than an SDR's power-of-two-table-compatible native rate by inserting a halfband decimation stage between the `IqSource` and the channelizer, exposed as `--capture-rate-hz`.

**Architecture:** A new `manta-dsp::decimate::Decimator` (cascaded Kaiser-windowed halfband FIR decimate-by-2 stages) wrapped by a new `manta-input::DecimatingSource` (implements `IqSource`, reports the decimated rate). `manta-cli` wraps whatever live source it opens (kiwi/soapy/hpsdr/audio) with `DecimatingSource` whenever `--capture-rate-hz` differs from the source's native rate — one generic wrap point, mirroring the existing `FixedCenterFreqSource` pattern, applied uniformly regardless of source type. `manta-engine::listen` needs zero changes: it already only calls `src.sample_rate()` once and builds the channelizer from whatever comes back.

**Tech Stack:** Rust workspace; `num-complex` for `Complex32`/`Complex64`; existing Kaiser-window machinery in `manta-dsp::proto` (`bessel_i0`, `KAISER_BETA`) reused for the halfband design; `manta-testkit` for golden-vector IQ synthesis; `hound` (via `manta-testkit::wav`) for WAV fixture writing; `clap` for CLI flags.

**Spec:** `docs/superpowers/specs/2026-09-09-variable-width-capture-design.md`

## Global Constraints

- Only exact power-of-two decimation factors are supported — no fractional/arbitrary-ratio resampling (design §1, "Not yet ... general resampling").
- The decimated output rate must itself satisfy the channelizer's `fs/93.75` power-of-two table constraint (same rule `Channelizer::new`/`SingleChannelExtractor::new` already enforce) — validated at construction, failing fast with a clean `Err`, never a panic.
- `AudioIqSource` is not specially excluded from the CLI wiring (a design refinement from the brainstormed spec, noted in Task 3): the single generic wrap point applies uniformly to whatever `open_source` returns, exactly like the existing `FixedCenterFreqSource` wrap already does. This is simpler than per-branch special-casing and is a no-op whenever `--capture-rate-hz` is omitted or matches the source's native rate.
- Sequential f64 accumulation for all new filter math (SPEC §6.4's repo-wide determinism convention — every existing filter in `manta-dsp` follows this: `proto.rs`, `channelizer.rs`, `hilbert.rs`, `single.rs`).
- No behavior change when `--capture-rate-hz` is omitted (defaults preserve today's exact behavior).
- New code follows this repo's existing per-module validation-duplication convention: `channelizer.rs` and `single.rs` already each independently duplicate the `fs/93.75` power-of-two check rather than sharing a helper; the new `decimate.rs` does the same, not a violation of this codebase's own DRY conventions.

---

### Task 1: `manta-dsp::decimate` — halfband decimator

**Files:**
- Create: `crates/manta-dsp/src/decimate.rs`
- Modify: `crates/manta-dsp/src/lib.rs` (add `pub mod decimate;`)

**Interfaces:**
- Produces: `pub struct Decimator`, `Decimator::new(fs_in: f64, factor: usize) -> Result<Self, String>`, `Decimator::fs_out(&self) -> f64`, `Decimator::process(&mut self, iq: &[num_complex::Complex32]) -> Vec<num_complex::Complex32>`. Also `pub const HALFBAND_TAPS: usize` and `pub fn design_halfband(taps: usize) -> Vec<f32>` (unit-tested directly, same pattern as `proto::design_prototype`).
- Consumes: `crate::proto::{bessel_i0, KAISER_BETA}` (existing, unchanged).

- [ ] **Step 1: Write the failing tests for `design_halfband`**

```rust
// crates/manta-dsp/src/decimate.rs (new file)

//! Power-of-two decimation stage between an IqSource and the channelizer
//! (issue #169): a cascade of halfband FIR decimate-by-2 stages, letting a
//! capture run at a narrower effective bandwidth than the SDR's native
//! rate while still landing on a channelizer-table-compatible rate
//! (`fs/93.75` a power of two, SPEC §1.1).

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1, same constant as channelizer.rs/single.rs.

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Number of taps in each halfband decimate-by-2 stage. Odd length,
/// Kaiser-windowed at the channelizer prototype's 80 dB target (SPEC
/// §1.2's `KAISER_BETA`) so the decimator isn't the weakest link in the
/// alias-rejection chain. Starting value; raised in Step 2 below if the
/// stopband test doesn't clear -78 dB.
pub const HALFBAND_TAPS: usize = 127;

/// Design one halfband lowpass stage: ideal cutoff at fs_in/4 (the new
/// Nyquist after decimate-by-2), Kaiser-windowed. By construction every
/// even-offset tap except the center is exactly zero (classic halfband
/// property: sinc(k/2) = 0 for even nonzero k).
pub fn design_halfband(taps: usize) -> Vec<f32> {
    assert!(taps % 2 == 1, "halfband design requires odd length");
    let center = (taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h = vec![0.0f64; taps];
    let mut sum = 0.0f64;
    for (i, tap) in h.iter_mut().enumerate() {
        let k = i as f64 - center;
        let ideal = 0.5 * sinc(k / 2.0);
        let t = 2.0 * i as f64 / (taps - 1) as f64 - 1.0;
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = ideal * w;
        sum += *tap;
    }
    h.iter().map(|&v| (v / sum) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |H(f)| in dB at frequency f (Hz), direct DTFT -- same helper shape
    /// as proto.rs's `response_db` (test-only, exact not FFT-approximated).
    fn response_db(h: &[f32], f_hz: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &tap) in h.iter().enumerate() {
            let phi = -2.0 * std::f64::consts::PI * f_hz * i as f64 / fs;
            re += tap as f64 * phi.cos();
            im += tap as f64 * phi.sin();
        }
        10.0 * (re * re + im * im).log10()
    }

    #[test]
    fn halfband_is_symmetric_and_unity_dc() {
        let h = design_halfband(HALFBAND_TAPS);
        assert_eq!(h.len(), HALFBAND_TAPS);
        for i in 0..h.len() / 2 {
            assert_eq!(h[i], h[h.len() - 1 - i], "tap {i} asymmetric");
        }
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        assert!((sum - 1.0).abs() < 1e-6, "DC gain {sum}");
    }

    #[test]
    fn halfband_even_offset_taps_are_zero() {
        // Classic halfband property: every tap at an even offset from the
        // center (except the center itself) is exactly zero.
        let h = design_halfband(HALFBAND_TAPS);
        let center = (h.len() - 1) / 2;
        for k in (2..center).step_by(2) {
            assert!(h[center + k].abs() < 1e-6, "tap at +{k} not ~0: {}", h[center + k]);
            assert!(h[center - k].abs() < 1e-6, "tap at -{k} not ~0: {}", h[center - k]);
        }
    }

    #[test]
    fn halfband_stopband_at_least_78_db() {
        // fs = 192 kHz -> new Nyquist (cutoff) at 48 kHz. Check well past
        // the cutoff, out to the original Nyquist (96 kHz).
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        let mut worst = -300.0f64;
        let mut f = 60_000.0; // guard band above the 48 kHz cutoff
        while f < 96_000.0 {
            worst = worst.max(response_db(&h, f, fs));
            f += 500.0;
        }
        assert!(worst <= -78.0, "worst stopband {worst} dB (raise HALFBAND_TAPS if this fails)");
    }

    #[test]
    fn halfband_passband_is_flat_near_dc() {
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        // Well inside the 48 kHz cutoff; DC-normalized gain should sit
        // close to 0 dB.
        let gain_db = response_db(&h, 5_000.0, fs);
        assert!(gain_db.abs() < 0.5, "passband gain {gain_db} dB");
    }
}
```

- [ ] **Step 2: Run the tests, verify pass or raise `HALFBAND_TAPS`**

Run: `cargo test -p manta-dsp decimate:: -- --nocapture`

Expected: all four pass. If `halfband_stopband_at_least_78_db` fails, raise `HALFBAND_TAPS` by 32 (keep it odd — add 33, not 32, since 127 is odd) and rerun. This is expected empirical tuning, the same workflow `proto.rs`'s own Kaiser design went through (see its `dump_reference_taps`/`reference_taps_pinned` pair) — not a placeholder.

- [ ] **Step 3: Commit the filter design**

```bash
git add crates/manta-dsp/src/decimate.rs
git commit -m "feat(manta-dsp): MAN-169 -- Kaiser-windowed halfband decimation filter design"
```

- [ ] **Step 4: Write the failing tests for the streaming `Decimator`**

Add to `crates/manta-dsp/src/decimate.rs`, above the existing `#[cfg(test)] mod tests` block:

```rust
/// One halfband decimate-by-2 stage: direct-form FIR, computed only at
/// kept (every-other) input instants -- never spends a multiply-
/// accumulate on a sample that will be dropped.
struct HalfbandStage {
    taps: Vec<f32>,
    hist: std::collections::VecDeque<Complex32>,
    parity: u64,
}

impl HalfbandStage {
    fn new(taps: Vec<f32>) -> Self {
        let len = taps.len();
        HalfbandStage {
            taps,
            hist: std::collections::VecDeque::from(vec![Complex32::new(0.0, 0.0); len]),
            parity: 0,
        }
    }

    fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len() / 2 + 1);
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            let keep = self.parity % 2 == 0;
            self.parity = self.parity.wrapping_add(1);
            if keep {
                // Sequential f64 accumulation (SPEC §6.4 determinism convention).
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (&h, &s) in self.taps.iter().zip(self.hist.iter()) {
                    re += h as f64 * s.re as f64;
                    im += h as f64 * s.im as f64;
                }
                out.push(Complex32::new(re as f32, im as f32));
            }
        }
        out
    }
}

/// Cascaded halfband decimator: `factor` (a power of two) stages of
/// decimate-by-2, front to back. SPEC-decode-core.md's decimation section.
pub struct Decimator {
    stages: Vec<HalfbandStage>,
    fs_out: f64,
}

impl Decimator {
    /// `factor` must be a power of two, and `fs_in / factor` must itself
    /// satisfy the channelizer's `fs/93.75` power-of-two table constraint
    /// (same validation shape as `Channelizer::new`/
    /// `SingleChannelExtractor::new`) -- a bad target rate fails here, at
    /// construction, not silently downstream.
    pub fn new(fs_in: f64, factor: usize) -> Result<Self, String> {
        if factor == 0 || !factor.is_power_of_two() {
            return Err(format!("decimation factor {factor} must be a power of two"));
        }
        let fs_out = fs_in / factor as f64;
        let nf = fs_out / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!(
                "unsupported target rate {fs_out}: fs/93.75 must be a power of two"
            ));
        }
        let n_stages = factor.trailing_zeros() as usize;
        let taps = design_halfband(HALFBAND_TAPS);
        let stages = (0..n_stages).map(|_| HalfbandStage::new(taps.clone())).collect();
        Ok(Decimator { stages, fs_out })
    }

    /// The decimated output rate, Hz.
    pub fn fs_out(&self) -> f64 {
        self.fs_out
    }

    /// Feed input IQ at fs_in; returns however many decimated samples
    /// became available (possibly 0, if too few input samples have
    /// accumulated through every cascade stage so far).
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        let mut cur = iq.to_vec();
        for stage in &mut self.stages {
            cur = stage.process(&cur);
        }
        cur
    }
}
```

Add to the `tests` module:

```rust
    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        assert!(Decimator::new(192_000.0, 3).is_err());
        assert!(Decimator::new(192_000.0, 4).is_ok());
    }

    #[test]
    fn rejects_target_rate_failing_channelizer_table_constraint() {
        // fs_in=100_000, factor=2 (a power of two) -> fs_out=50_000, whose
        // fs/93.75 = 533.33.. is not an integer, let alone a power of two.
        assert!(Decimator::new(100_000.0, 2).is_err());
    }

    #[test]
    fn fs_out_matches_factor() {
        let d = Decimator::new(192_000.0, 4).unwrap();
        assert_eq!(d.fs_out(), 48_000.0);
    }

    #[test]
    fn dc_tone_passes_through_decimation_at_near_unity() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // A tone well inside the final 48 kHz/2=24 kHz Nyquist.
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        // Skip transient: two cascaded HALFBAND_TAPS-length filters need
        // roughly HALFBAND_TAPS output samples (post-decimation) to settle.
        let warmup = HALFBAND_TAPS;
        assert!(out.len() > warmup + 50, "only {} output samples", out.len());
        for s in &out[warmup..] {
            assert!((s.norm() - 1.0).abs() < 0.1, "mag {}", s.norm());
        }
    }

    #[test]
    fn tone_above_new_nyquist_is_rejected() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // 30 kHz is above the final 24 kHz Nyquist -- must be suppressed,
        // not aliased through as a spurious in-band tone.
        let iq = tone(30_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        let warmup = HALFBAND_TAPS;
        for s in &out[warmup..] {
            assert!(s.norm() < 0.05, "aliased mag {}", s.norm());
        }
    }

    #[test]
    fn is_deterministic() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 40_000, 1.0, fs_in);
        let mut d_a = Decimator::new(fs_in, 4).unwrap();
        let mut d_b = Decimator::new(fs_in, 4).unwrap();
        let out_a = d_a.process(&iq);
        let out_b = d_b.process(&iq);
        assert_eq!(out_a.len(), out_b.len());
        for (a, b) in out_a.iter().zip(out_b.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let mut whole = Decimator::new(fs_in, 4).unwrap();
        let out_whole = whole.process(&iq);

        let mut chunked = Decimator::new(fs_in, 4).unwrap();
        let mut out_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            out_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(out_whole.len(), out_chunked.len());
        for (a, b) in out_whole.iter().zip(out_chunked.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }
```

- [ ] **Step 5: Run the tests, verify pass**

Run: `cargo test -p manta-dsp decimate:: -- --nocapture`

Expected: all pass. If `dc_tone_passes_through_decimation_at_near_unity` or `tone_above_new_nyquist_is_rejected` fail on the tolerance bounds, widen the tolerance slightly (these are empirically-measured bounds, same as `channelizer.rs`'s own `0.05`/`2e-4` tolerances) rather than changing the filter design, unless the stopband/passband unit tests from Step 1 also regressed.

- [ ] **Step 6: Wire the module into `manta-dsp`'s lib root**

In `crates/manta-dsp/src/lib.rs`, add:

```rust
pub mod decimate;
```

(alongside the existing `pub mod channelizer;`, `pub mod floor;`, etc., alphabetically between `channelizer` and `floor`).

- [ ] **Step 7: Run the full `manta-dsp` test suite**

Run: `cargo test -p manta-dsp`

Expected: all tests pass, including the pre-existing `channelizer`/`proto`/`single`/`hilbert`/`floor` suites (unaffected).

- [ ] **Step 8: Commit**

```bash
git add crates/manta-dsp/src/decimate.rs crates/manta-dsp/src/lib.rs
git commit -m "feat(manta-dsp): MAN-169 -- cascaded halfband Decimator (power-of-two rate conversion)"
```

---

### Task 2: `manta-input::DecimatingSource`

**Files:**
- Create: `crates/manta-input/src/decimate.rs`
- Modify: `crates/manta-input/src/lib.rs` (add `pub mod decimate;` and `pub use decimate::DecimatingSource;`)

**Interfaces:**
- Consumes: `manta_dsp::decimate::Decimator` (Task 1), `crate::IqSource` trait (`sample_rate`, `center_freq_hz`, `read`, `confirmed_live_handle`, `health_counters` — all defined in `crates/manta-input/src/lib.rs`).
- Produces: `pub struct DecimatingSource`, `DecimatingSource::new(inner: Box<dyn IqSource>, target_rate_hz: f64) -> anyhow::Result<Self>`, implementing `IqSource`. Consumed by `manta-cli` in Task 3.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/manta-input/src/decimate.rs (new file)

//! Wraps any `IqSource` with a `manta_dsp::decimate::Decimator`, reporting
//! the decimated rate as its own `sample_rate()` -- issue #169. Composes
//! transparently with every existing `IqSource` impl (kiwi/soapy/hpsdr/
//! audio); `manta-engine::listen` needs no changes since it only ever
//! calls `sample_rate()` once, up front.

use crate::{InputHealthCounters, IqSource};
use anyhow::{bail, Result};
use manta_dsp::decimate::Decimator;
use num_complex::Complex32;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct DecimatingSource {
    inner: Box<dyn IqSource>,
    decimator: Decimator,
    fs_out: f64,
    factor: usize,
    /// Decimated samples produced by a prior inner read that didn't fit
    /// in the caller's buffer -- carried over so no decimated sample is
    /// ever dropped just because the caller's buffer was smaller than one
    /// inner read happened to produce.
    pending: VecDeque<Complex32>,
}

impl DecimatingSource {
    /// Wrap `inner` (reporting `inner.sample_rate()`, Hz) with a decimator
    /// targeting `target_rate_hz`. Errors (non-power-of-two factor,
    /// non-table target rate) match `Decimator::new`'s.
    pub fn new(inner: Box<dyn IqSource>, target_rate_hz: f64) -> Result<Self> {
        let fs_in = inner.sample_rate();
        let factor = (fs_in / target_rate_hz).round() as usize;
        if factor == 0 || (fs_in / factor as f64 - target_rate_hz).abs() > 1e-6 {
            bail!(
                "--capture-rate-hz {target_rate_hz} does not evenly divide the source's \
                 native rate {fs_in} by a power of two"
            );
        }
        let decimator = Decimator::new(fs_in, factor).map_err(|e| anyhow::anyhow!(e))?;
        let fs_out = decimator.fs_out();
        Ok(DecimatingSource {
            inner,
            decimator,
            fs_out,
            factor,
            pending: VecDeque::new(),
        })
    }
}

impl IqSource for DecimatingSource {
    fn sample_rate(&self) -> f64 {
        self.fs_out
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        while self.pending.is_empty() {
            let mut raw = vec![Complex32::new(0.0, 0.0); buf.len().max(1) * self.factor];
            let n = self.inner.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            let decimated = self.decimator.process(&raw[..n]);
            self.pending.extend(decimated);
        }
        let n = buf.len().min(self.pending.len());
        for (slot, s) in buf[..n].iter_mut().zip(self.pending.drain(..n)) {
            *slot = s;
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory IqSource test double, mirroring
    /// `manta-engine::listen`'s own `FixedFreqSource` test helper.
    struct InMemorySource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        live: Option<Arc<AtomicBool>>,
    }

    impl IqSource for InMemorySource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
        fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
            self.live.clone()
        }
    }

    fn tone(freq: f64, n: usize, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn sample_rate_reports_the_decimated_value() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
    }

    #[test]
    fn center_freq_and_liveness_forward_to_the_inner_source() {
        let live = Arc::new(AtomicBool::new(true));
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_035_000.0,
            live: Some(live.clone()),
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        assert_eq!(src.center_freq_hz(), 14_035_000.0);
        assert!(src.confirmed_live_handle().unwrap().load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn read_yields_roughly_input_len_over_factor_samples_then_eof() {
        let fs_in = 192_000.0;
        let n_in = 40_000;
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(2_000.0, n_in, fs_in),
            cursor: 0,
            fs: fs_in,
            center_freq_hz: 0.0,
            live: None,
        });
        let mut src = DecimatingSource::new(inner, 48_000.0).unwrap(); // factor 4
        let mut total = 0usize;
        let mut buf = vec![Complex32::new(0.0, 0.0); 500];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        // factor 4: ~n_in/4 output samples, within a cascade's worth of
        // rounding/warm-up slack either side.
        let expected = n_in / 4;
        assert!(
            (total as i64 - expected as i64).unsigned_abs() < 200,
            "total {total}, expected ~{expected}"
        );
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: vec![],
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 0.0,
            live: None,
        });
        assert!(DecimatingSource::new(inner, 70_000.0).is_err());
    }

    #[test]
    fn rejects_target_rate_failing_channelizer_table_constraint() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: vec![],
            cursor: 0,
            fs: 100_000.0,
            center_freq_hz: 0.0,
            live: None,
        });
        assert!(DecimatingSource::new(inner, 50_000.0).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (module doesn't exist yet)**

Run: `cargo test -p manta-input decimate:: -- --nocapture`

Expected: FAIL with "unresolved module `decimate`" or similar, until Step 3's `lib.rs` edit lands.

- [ ] **Step 3: Wire the module into `manta-input`'s lib root**

In `crates/manta-input/src/lib.rs`, add (after the existing `kiwi` block, before the `#[cfg(feature = "soapy")]` block, since this module is unconditional — it wraps any `IqSource` generically, not tied to any one transport feature):

```rust
pub mod decimate;
pub use decimate::DecimatingSource;
```

- [ ] **Step 4: Run the tests, verify pass**

Run: `cargo test -p manta-input decimate:: -- --nocapture`

Expected: all 5 tests pass.

- [ ] **Step 5: Run the full `manta-input` test suite**

Run: `cargo test -p manta-input`

Expected: all pass, including pre-existing `soapy`/`audio`/`kiwi`/`hpsdr` suites (unaffected). Note: `soapy`/`hpsdr` suites only run with `--features soapy,hpsdr`; run `cargo test -p manta-input --features soapy,hpsdr` too if those toolchains/libs are available locally, otherwise this is covered by CI.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-input/src/decimate.rs crates/manta-input/src/lib.rs
git commit -m "feat(manta-input): MAN-169 -- DecimatingSource wraps any IqSource with a Decimator"
```

---

### Task 3: `manta-cli` — `--capture-rate-hz` wiring

**Files:**
- Modify: `crates/manta-cli/src/main.rs` (add `capture_rate_hz` field to the `Run`, `Soak`, and `Doctor` `Commands` variants; add a `maybe_decimate` helper; call it at each subcommand's existing source-wrapping point, alongside the existing `dial_freq_hz`/`FixedCenterFreqSource` wrap)
- Modify: `crates/manta-cli/tests/cli.rs` (new tests)

**Interfaces:**
- Consumes: `manta_input::DecimatingSource::new` (Task 2), the existing `FixedCenterFreqSource` wrap pattern and `IqSource` trait already in scope in `main.rs`.
- Produces: `--capture-rate-hz <F64>` CLI flag on `run`/`listen` (alias), `soak`, and `doctor`.

- [ ] **Step 1: Write the failing CLI tests**

Add to `crates/manta-cli/tests/cli.rs` (near the other `run --source` error-path tests, e.g. after `deprecated_daemon_spelling_warns_on_stderr_and_names_the_replacement`):

```rust
#[test]
fn capture_rate_hz_that_does_not_evenly_divide_the_source_rate_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1(); // fs = 96_000.0
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "70000"]) // 96000/70000 is not an integer
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--capture-rate-hz") || stderr.contains("power of two"),
        "stderr: {stderr}"
    );
}

#[test]
fn capture_rate_hz_that_divides_evenly_decimates_and_still_decodes() {
    let dir = tempfile::tempdir().unwrap();
    // A short scene (not the full 120 s V1 duration) so this CLI test
    // stays fast -- proving the --capture-rate-hz wiring runs end-to-end
    // is this test's job, not full decode-accuracy (that's Task 4's
    // golden vector).
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 10.0;
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "48000"]) // 96000 -> 48000, factor 2
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p manta-cli capture_rate_hz -- --nocapture`

Expected: FAIL with "unexpected argument '--capture-rate-hz'" (clap doesn't know the flag yet).

- [ ] **Step 3: Add the `capture_rate_hz` field to `Run`, `Soak`, and `Doctor`**

In `crates/manta-cli/src/main.rs`, inside each of the three `Commands::Run { ... }`, `Commands::Soak { ... }`, `Commands::Doctor { ... }` variant definitions, add one new field (place it near the other RF-source flags, e.g. right after the `soapy_gain`/`hpsdr_rate` block in each — exact position among sibling fields doesn't matter for named-field structs):

```rust
        /// Decimate the source down to this rate before the channelizer
        /// (issue #169) -- must evenly divide the source's native rate by
        /// a power of two, and the result must itself be a valid
        /// channelizer table rate (fs/93.75 a power of two). Omit to use
        /// the source's native rate unchanged (today's behavior).
        #[arg(long)]
        capture_rate_hz: Option<f64>,
```

- [ ] **Step 4: Add the `maybe_decimate` helper**

In `crates/manta-cli/src/main.rs`, near `FixedCenterFreqSource`'s definition, add:

```rust
/// Wrap `src` in a `DecimatingSource` targeting `capture_rate_hz`, unless
/// it's `None` or already matches the source's native rate (a no-op in
/// either case -- omitting `--capture-rate-hz` reproduces today's exact
/// behavior). Applied uniformly regardless of source type (kiwi/soapy/
/// hpsdr/audio/file replay), mirroring how `dial_freq_hz`'s
/// `FixedCenterFreqSource` wrap is already applied uniformly below.
fn maybe_decimate(src: Box<dyn IqSource>, capture_rate_hz: Option<f64>) -> Result<Box<dyn IqSource>> {
    match capture_rate_hz {
        Some(target) if (target - src.sample_rate()).abs() > 1e-6 => {
            Ok(Box::new(manta_input::DecimatingSource::new(src, target)?))
        }
        _ => Ok(src),
    }
}
```

- [ ] **Step 5: Call `maybe_decimate` at each subcommand's source-wrapping point**

In `Commands::Run`'s match arm, immediately before the existing:

```rust
            let src: Box<dyn IqSource> = match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            };
```

insert:

```rust
            let src: Box<dyn IqSource> = maybe_decimate(src, capture_rate_hz)?;
```

and add `capture_rate_hz,` to that match arm's destructuring list at the top (alongside `dial_freq_hz`, `replay_epoch`, etc., where the `Commands::Run { ... }` fields are pattern-matched).

In `Commands::Soak`'s match arm, add `capture_rate_hz,` to its destructuring list (alongside `hpsdr_rate`, before the closing `} => {`), then change:

```rust
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
```

to:

```rust
            let src: Box<dyn IqSource> = maybe_decimate(src, capture_rate_hz)?;
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
```

(Soak has no `dial_freq_hz`-style wrap today — `src` from the `open_hpsdr_source`/`open_source` dispatch is used directly, so this inserts the one new wrap line right before its first use.)

In `Commands::Doctor`'s match arm, add `capture_rate_hz,` to its destructuring list (alongside `hpsdr_rate`, before `json,`), then change:

```rust
            let report = manta_engine::doctor(src, &cfg, std::time::Duration::from_secs(duration))?;
```

to:

```rust
            let src: Box<dyn IqSource> = maybe_decimate(src, capture_rate_hz)?;
            let report = manta_engine::doctor(src, &cfg, std::time::Duration::from_secs(duration))?;
```

- [ ] **Step 6: Run the tests, verify pass**

Run: `cargo test -p manta-cli capture_rate_hz -- --nocapture`

Expected: both tests pass.

- [ ] **Step 7: Run the full `manta-cli` test suite**

Run: `cargo test -p manta-cli`

Expected: all pass, including every pre-existing `cli.rs`/`golden_*.rs` test (unaffected, since `capture_rate_hz` defaults to `None` everywhere it wasn't explicitly set).

- [ ] **Step 8: Commit**

```bash
git add crates/manta-cli/src/main.rs crates/manta-cli/tests/cli.rs
git commit -m "feat(manta-cli): MAN-169 -- --capture-rate-hz flag on run/soak/doctor"
```

---

### Task 4: Golden vector — decimated 48 kHz clean-signal decode

**Files:**
- Create: `crates/manta-cli/tests/golden_decimated_capture.rs`
- Modify: `crates/manta-cli/Cargo.toml` (add `manta-dsp` to `[dev-dependencies]`)

**Interfaces:**
- Consumes: `manta_testkit::vectors::{v1, render, RenderedVector}` (existing), `manta_dsp::decimate::Decimator` (Task 1), `manta_testkit::wav::write_fixture` (existing), `manta_testkit::cer::cer` (existing, used by `golden_v1.rs`).
- Produces: a new golden-vector test proving the decimator's DSP output decodes correctly end-to-end, distinct from `channelizer_multisignal.rs`'s coverage (which proves the channelizer generalizes across table sizes fed *clean-at-that-rate* IQ, not IQ that was captured at one rate and decimated down).

- [ ] **Step 1: Add `manta-dsp` to `manta-cli`'s dev-dependencies**

In `crates/manta-cli/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
manta-dsp = { workspace = true }
```

- [ ] **Step 2: Write the failing golden-vector test**

```rust
// crates/manta-cli/tests/golden_decimated_capture.rs

//! New golden vector (issue #169): a clean-20-style scene synthesized at
//! 192 kHz, decimated through the real `manta_dsp::decimate::Decimator`
//! cascade down to 48 kHz, then decoded end-to-end via `manta decode`.
//! Proves the decimator itself doesn't corrupt decode -- distinct from
//! `channelizer_multisignal.rs`, which only proves the channelizer
//! generalizes across table sizes when fed IQ synthesized clean at that
//! rate to begin with, never IQ that was captured at one rate and
//! decimated down (the actually-new code path here).

use manta_dsp::decimate::Decimator;
use num_complex::Complex32;
use std::process::Command;

#[test]
fn clean_signal_decodes_correctly_after_192khz_to_48khz_decimation() {
    // Same clean-20 signal shape as V1 (20 WPM, +20 dB, offset +12.34 kHz,
    // W1AW), but synthesized at 192 kHz so a real decimate-by-4 cascade
    // is exercised (V1 itself is defined at 96 kHz).
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 192_000.0;
    let rendered = manta_testkit::vectors::render(&spec).unwrap();

    let mut decimator = Decimator::new(192_000.0, 4).unwrap();
    let decimated: Vec<Complex32> = decimator.process(&rendered.samples);
    assert_eq!(decimator.fs_out(), 48_000.0);

    let dir = tempfile::tempdir().unwrap();
    manta_testkit::wav::write_fixture(
        dir.path(),
        "decimated_v1",
        &decimated,
        48_000.0,
        spec.center_freq_hz,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join("decimated_v1.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // Same V1 pass criteria (golden_v1.rs): char accuracy, 1 track, freq
    // error <= 10 Hz. The Decimator's own filter group delay shifts the
    // effective start of the signal slightly but does not change its
    // carrier frequency, so the same expected_freq_hz applies.
    let decoded = report["text"].as_str().unwrap();
    let cer_val = manta_testkit::cer::cer(&rendered.keyed_texts[0], decoded);
    assert!(
        cer_val < 0.02,
        "decimated V1 char accuracy must be >= 98% (CER < 0.02), got CER {cer_val:.4}\n\
         expected: {}\ndecoded:  {}",
        rendered.keyed_texts[0],
        decoded
    );

    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - rendered.expected_freq_hz).abs() <= 10.0,
        "freq {} expected {} (err {})",
        freq,
        rendered.expected_freq_hz,
        (freq - rendered.expected_freq_hz).abs()
    );

    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails or passes as-is**

Run: `cargo test -p manta-cli --test golden_decimated_capture -- --nocapture`

Expected: this test exercises only already-implemented code (Tasks 1-3 are already merged into this branch by this point), so it should compile and run immediately. If the CER or freq-error assertion fails, investigate empirically the same way `golden_v1.rs`'s own comments document (its CER bound of `0.02` and freq bound of `10.0` were both measured, not assumed) — rerun with `-- --nocapture` and inspect `decoded`/`freq` in the assertion failure message; widen the tolerance only if the discrepancy is explained by the decimator's own group delay/passband ripple (expected and acceptable), not by a real decode regression.

- [ ] **Step 4: Run the full `manta-cli` test suite**

Run: `cargo test -p manta-cli`

Expected: all pass, including the new test and every pre-existing `golden_*.rs` file.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-cli/Cargo.toml crates/manta-cli/tests/golden_decimated_capture.rs
git commit -m "test(manta-cli): MAN-169 -- golden vector for 192kHz-decimated-to-48kHz decode"
```

---

### Task 5: Docs

**Files:**
- Modify: `ARCHITECTURE.md` (§3 input layer)
- Modify: `docs/SPEC-decode-core.md` (new decimation subsection, §7 vector table, §9 config-key table)
- Modify: `CLAUDE.md` (Status section)

**Interfaces:** none (docs only).

- [ ] **Step 1: Update `ARCHITECTURE.md` §3**

Read the current §3 section first (`grep -n "## 3. Input layer" -A 40 ARCHITECTURE.md`), then add a short paragraph after the existing `IqSource` trait description and before or after the "Sample-rate assumptions" paragraph, along these lines (adapt wording to fit the surrounding prose exactly, don't just append disconnected text):

```markdown
**Variable-width capture** (issue #169): an optional decimation stage
(`manta-dsp::decimate::Decimator`, wrapped as `manta-input::
DecimatingSource`) sits between a live `IqSource` and the channelizer.
Operators select a narrower effective capture rate via
`--capture-rate-hz`; the SDR still opens at its best native rate, and a
cascade of Kaiser-windowed halfband FIR decimate-by-2 stages narrows it
down before the channelizer ever sees it. Only exact power-of-two
factors are supported (no general resampling), and the resulting rate
must itself satisfy the channelizer's `fs/93.75` table constraint.
Motivated by field evidence (docs/DECISIONS/2026-09-09-soapy-gain-is-
inverted-attenuation-scale.md, docs/DECISIONS/2026-09-09-20m-dial-shift-
edge-artifact-confirmed.md) that a narrower capture bandwidth can
improve real-signal detection on some hardware.
```

- [ ] **Step 2: Update `docs/SPEC-decode-core.md`**

Read §1 and §9 first (`grep -n "^## 1\." -A 5 docs/SPEC-decode-core.md`, `grep -n "^## 9\." -A 30 docs/SPEC-decode-core.md`) to match heading style exactly, then:

(a) Add a new `### 1.5 Decimation (variable-width capture)` subsection after the existing `### 1.4 Fine frequency estimate` and before `## 2. Noise floor & signal-presence detection`:

```markdown
### 1.5 Decimation (variable-width capture, issue #169)

An optional stage between the `IqSource` and the channelizer: a cascade
of `log2(factor)` Kaiser-windowed halfband FIR decimate-by-2 stages,
`factor` restricted to powers of two. Each stage's ideal cutoff sits at
its own input Nyquist/2 (the output Nyquist after that stage's
decimate-by-2), Kaiser-windowed at the same beta/stopband target as the
channelizer prototype (§1.2's `KAISER_BETA`, 80 dB). The decimated rate
must itself satisfy §1.1's `fs/93.75` power-of-two table constraint.
Module: `manta-dsp::decimate`.
```

(b) Add a row to the §7 golden-vector table, after the V10 row:

```markdown
| V31 | decimated-clean-20 | Same scene as V1 (20 WPM, +20 dB, offset +12.34 kHz, W1AW), synthesized at 192 kHz then decimated to 48 kHz via `manta_dsp::decimate::Decimator` | AWGN only, no jitter | char ≥ 98%; 1 track; freq error ≤ 10 Hz (same pass criteria as V1) |
```

And update the line just below the table (`M0 = V1 passing end-to-end from a WAV file. M1 = V1–V6. V7–V10 and V8w gate M2...`) to mention V31 gates the decimation feature specifically, e.g. append: `V31 gates variable-width capture (issue #169).`

(c) Add a config key to the §9 table, under the existing `[input]` block:

```toml
[input]
# ... existing freq_correction_ppm ...
# Target post-decimation capture rate, Hz (issue #169). None (the
# default) uses the source's native rate unchanged. Must evenly divide
# the source's native rate by a power of two, and must itself satisfy
# fs/93.75 being a power of two. manta_dsp::decimate::Decimator,
# manta_input::DecimatingSource.
capture_rate_hz = none
```

- [ ] **Step 3: Update `CLAUDE.md`'s Status section**

Read the current Status paragraph first, then append one clause noting variable-width capture landed, in the same terse style as the existing sentences, e.g.: "Variable-width capture (issue #169, `--capture-rate-hz`) implemented -- see docs/superpowers/specs/2026-09-09-variable-width-capture-design.md." Keep the file under ~100 lines per this repo's own instruction-file-discipline rule -- trim an equally-terse older clause if needed to make room, don't just grow the file unboundedly.

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md docs/SPEC-decode-core.md CLAUDE.md
git commit -m "docs: MAN-169 -- document variable-width capture (decimation stage, V31, config key)"
```

---

### Task 6: Final verification and PR update

**Files:** none (verification only).

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`

Expected: all pass. If `soapy`/`hpsdr` features are available locally, also run `cargo test --workspace --features soapy,hpsdr`.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: clean. Fix any warnings the new code introduced (e.g. `needless_range_loop`, unused `mut`) before proceeding.

- [ ] **Step 3: Push and confirm the draft PR's CI**

```bash
git push
```

PR #170 already exists (opened during brainstorming as the claim on issue #169); this push updates it. Per this repo's auto-merge policy, confirm `gh pr checks 170` goes green, then mark the PR ready for review (`gh pr ready 170`) — auto-merge (`gh pr merge --auto --squash`) was not yet set on this draft PR and should be set now that it's ready.

- [ ] **Step 4: Enable auto-merge**

```bash
gh pr ready 170
gh pr merge 170 --auto --squash
```
