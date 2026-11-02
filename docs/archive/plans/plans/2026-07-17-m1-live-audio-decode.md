# M1 — Live Audio, One Signal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `manta listen` decodes a live off-air CW signal from real audio (rig RX audio or a replayed WAV) end-to-end, continuously, gated on golden vectors V1–V6 (SPEC-decode-core §7) plus a manual W1AW copy run.

**Architecture:** New `AudioIqSource` (manta-input) converts real audio (via `coppa-audio`, live device or file replay) to analytic `Complex32` through a new FIR Hilbert transformer (manta-dsp), then feeds M0's already-streaming-capable `SingleChannelExtractor`/`TrackDecoder` pair through a new single-threaded loop (`manta-engine::listen`) — no PFB, no track pool, no actor/thread split (M1 has exactly one track). The same Hilbert transformer, applied in reverse (complex → real → coppa Watterson → real → complex), lets V4/V5's golden vectors use coppa's real, currently-shipped `watterson_preset()` API instead of the never-built streaming `WattersonChannel` proposal.

**Tech Stack:** Rust (edition 2021, rust-version 1.85.0), new git deps `coppa-audio`/`coppa-channel` (pinned alongside the existing `coppa-dsp` pin), `cpal` 0.18 (device I/O), `ctrlc` (graceful shutdown), `libc` (soak RSS sampling).

**Design doc:** `docs/superpowers/specs/2026-07-17-m1-live-audio-design.md` — read it first; this plan implements it section by section.

## Global Constraints

Copied from SPEC-decode-core.md, ARCHITECTURE.md, ROADMAP.md, CLAUDE.md, and the M1 design doc. Every task's requirements implicitly include this section.

- **Determinism (SPEC §6):** NO RNG and NO wall clock anywhere in the decode path (`manta-dsp`/`manta-decode`/`manta-engine`'s `listen`/`soak` decode loop). Per-sample state is `f32`; long accumulations (FIR dot products) run **sequentially in `f64`** — this applies to the new Hilbert FIR exactly as it already applies to the PFB prototype.
- **Timing constants:** channel output rate `fo = 375 Hz`; hop period `HOP_MS = 8/3 ms` (`manta_decode::{FO_HZ, HOP_MS}`). Unchanged by M1 — the extractor's rate math already generalizes to any `fs` with `fs/93.75` a power of two, and 48000/93.75 = 512 satisfies this exactly.
- **coppa reuse boundary (ARCHITECTURE §2):** `coppa-audio` (cpal-backed device I/O, file replay, resampling) is reused as-is. `coppa-channel::watterson_preset()` is reused for V4/V5 (real, one-shot API — see Task 9's deviation note). The Hilbert transformer is **new** code (`manta-dsp::hilbert`) — neither crate ships one.
- **Dependency pin:** bump the existing `coppa-dsp` git pin and add `coppa-audio`/`coppa-channel` pinned to the **same** rev, `f8a4d16df7e5776a0756943c05712038774e6c70` (coppa `origin/main` HEAD as of 2026-07-15, a descendant of the M0 pin and of the 2026-07-07 Watterson bug-fix commits `9ab1547`/`34aec5f`/`fc35895`). Record the bump in `docs/DECISIONS/` (Task 11).
- **No SoapySDR anywhere** (unchanged from M0; M1 doesn't touch this).
- **Licensing/metadata:** every new crate item inherits workspace `license = "MIT OR Apache-2.0"`, `edition = "2021"`, `rust-version = "1.85.0"`.
- **Commit `Cargo.lock`** after every dependency change.
- **Multi-agent hygiene (CLAUDE.md):** work on a branch (`feat/m1-live-audio`), push early, open a draft PR as the claim, `--force-with-lease` only, main moves only by PR merge.
- **CI:** GitHub Actions `ubuntu-latest` + `macos-latest`: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. The soak test (Task 10) must complete in CI time — it runs against a short-but-representative synthetic scene, not a literal wall-clock hour (see Task 10).
- Rustdoc comments on every public item cite the SPEC/ARCHITECTURE/design-doc section they implement.

## Deviations from the design doc (record in `docs/DECISIONS/` in Task 11)

1. **Soak harness does not track input-overrun.** The design doc (§7) assumed `AudioRingConsumer::overflow_count()` would be readable from the soak harness. It isn't: `coppa_audio::CpalSource` owns its ring internally and doesn't expose an overflow accessor on itself or on the `AudioSource` trait — there is no public API surface to read it from. File-replay sources (`WavSource`/`RawF32Source`, what CI's automated soak actually runs against) have no ring and cannot overrun by construction, so this gap doesn't block the CI gate; it blocks only live-hardware overrun observability, which is out of scope for M1's automated soak and would need a `coppa-audio` API addition (a real upstream ask, not made unilaterally here — CLAUDE.md's cross-repo contract rule). The soak harness checks panics and RSS growth only.
2. **The coppa commit pin lives in `Cargo.toml` + `docs/DECISIONS/`, not per-vector `.manifest.json`.** The design doc's §6 said "pinned in the vector's `.manifest.json`" — but M0's actual, already-established convention (see `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`'s "coppa dependency pin" section) pins the rev in `Cargo.toml`/`Cargo.lock` and records it in `docs/DECISIONS/`, not per-fixture. Following the real precedent instead of the design doc's phrasing.

---

### Task 1: Bump coppa dependency pin; add coppa-audio, coppa-channel, cpal, ctrlc, libc

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/manta-input/Cargo.toml`
- Modify: `crates/manta-engine/Cargo.toml`
- Modify: `crates/manta-cli/Cargo.toml`
- Modify: `crates/manta-testkit/Cargo.toml`

**Interfaces:**
- Produces: `coppa_audio::{AudioSource, AudioSink, CpalSource, ResamplingSource, WavSource, find_input_device_by_name}` and `coppa_channel::watterson::{watterson_preset, WattersonPreset}`, available to every crate that declares the new workspace deps below.

- [ ] **Step 1: Bump the coppa-dsp pin and add coppa-audio/coppa-channel to the workspace root**

Edit `Cargo.toml`, replacing the `coppa-dsp` line and adding two new lines directly after it:

```toml
coppa-dsp = { git = "https://github.com/HagaleTechnologies/coppa.git", rev = "f8a4d16df7e5776a0756943c05712038774e6c70" }
coppa-audio = { git = "https://github.com/HagaleTechnologies/coppa.git", rev = "f8a4d16df7e5776a0756943c05712038774e6c70" }
coppa-channel = { git = "https://github.com/HagaleTechnologies/coppa.git", rev = "f8a4d16df7e5776a0756943c05712038774e6c70" }

cpal = "0.18"
ctrlc = "3"
libc = "0.2"
```

- [ ] **Step 2: Add coppa-audio, cpal, and manta-dsp to manta-input**

Edit `crates/manta-input/Cargo.toml`'s `[dependencies]` block, adding:

```toml
coppa-audio = { workspace = true }
cpal = { workspace = true }
manta-dsp = { workspace = true }
```

(This adds the `manta-input → manta-dsp` edge for the shared Hilbert transformer — not in ARCHITECTURE.md's current diagram, whose prose already assigns Hilbert-to-analytic conversion to the input layer §3. Task 11 updates the diagram.)

- [ ] **Step 3: Add libc to manta-engine**

Edit `crates/manta-engine/Cargo.toml`'s `[dependencies]` block, adding:

```toml
libc = { workspace = true }
```

- [ ] **Step 4: Add manta-input, manta-decode, and ctrlc to manta-cli**

Edit `crates/manta-cli/Cargo.toml`'s `[dependencies]` block, adding:

```toml
ctrlc = { workspace = true }
manta-decode = { workspace = true }
manta-input = { workspace = true }
```

- [ ] **Step 5: Add coppa-channel to manta-testkit**

Edit `crates/manta-testkit/Cargo.toml`'s `[dependencies]` block, adding:

```toml
coppa-channel = { workspace = true }
```

- [ ] **Step 6: Build and commit**

Run: `cargo build --workspace 2>&1 | tail -30`
Expected: clean build (no code uses the new deps yet, so this only proves resolution/compilation of the dependency graph itself).

```bash
git add Cargo.toml Cargo.lock crates/manta-input/Cargo.toml crates/manta-engine/Cargo.toml crates/manta-cli/Cargo.toml crates/manta-testkit/Cargo.toml
git commit -m "chore: bump coppa pin, add coppa-audio/coppa-channel/cpal/ctrlc/libc deps"
```

---

### Task 2: Fix all-dah opener decode bug (pinned decision 20)

**Files:**
- Modify: `crates/manta-decode/src/timing.rs`
- Modify: `crates/manta-decode/src/decoder.rs` (regression test only)

**Interfaces:**
- Consumes: nothing new — this is a self-contained fix inside `ClusterPair`, already private to `timing.rs`.
- Produces: `SpeedTracker`'s public API (`ready`, `mu_dit_ms`, `mu_dah_ms`, `boundary_ms`, `wpm`, `on_mark`) is unchanged in signature; only its behavior on a homogeneous-dah opener changes.

- [ ] **Step 1: Write the failing unit tests**

In `crates/manta-decode/src/timing.rs`, inside `mod tests`, add:

```rust
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
    assert!((t.mu_dit_ms() - 60.0).abs() < 0.1, "mu_dit {}", t.mu_dit_ms());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p manta-decode unimodal_dah_init -- --nocapture`
Expected: FAIL — with the current code, the unimodal branch always assumes dits, so `mu_dah_ms()` after step 1's feed is `3 * mean ≈ 540.6`, not `≈180.2`.

- [ ] **Step 3: Implement the fix in `ClusterPair`**

In `crates/manta-decode/src/timing.rs`, replace the `ClusterPair` struct definition:

```rust
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
}
```

Replace `ClusterPair::new()`:

```rust
    fn new() -> Self {
        ClusterPair {
            lo: 0.0,
            hi: 0.0,
            init: Vec::with_capacity(5),
            ready: false,
            confirmed: false,
            placeholder_is_lo: false,
        }
    }
```

Replace `ClusterPair::observe()`:

```rust
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
```

Replace `ClusterPair::initialize()` (including its doc comment):

```rust
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
    fn initialize(&mut self) {
        let mut s = self.init.clone();
        s.sort_by(f32::total_cmp);
        if s[s.len() - 1] / s[0] >= 2.0 {
            // Split at the largest ratio gap between consecutive sorted values.
            let mut best_i = 0;
            let mut best_r = 0.0f32;
            for i in 0..s.len() - 1 {
                let r = s[i + 1] / s[i];
                if r > best_r {
                    best_r = r;
                    best_i = i;
                }
            }
            self.lo = mean(&s[..=best_i]);
            self.hi = mean(&s[best_i + 1..]);
            self.confirmed = true;
            self.placeholder_is_lo = false;
        } else {
            let m = mean(&s);
            if m > DIT_CLAMP_MS.1 {
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p manta-decode timing:: -- --nocapture`
Expected: PASS — all existing `timing.rs` tests (`initializes_bimodal_after_five_marks`, `unimodal_init_provisional_then_reanchors`, `ratio_constraint_reanchors_dah`, `dit_clamp_bounds_speed`, `step_speed_change_reinitializes`, `gap_classification_nominal`, `farnsworth_moves_word_threshold`) plus the two new tests all pass.

- [ ] **Step 5: Add the end-to-end regression test**

In `crates/manta-decode/src/decoder.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn all_dah_opener_decodes_correctly() {
        // Pinned decision 20 regression, exercised end-to-end. At 24
        // hops/dit (dit = 64 ms, ~18.75 WPM), a homogeneous run of dahs
        // averages 192 ms -- unambiguously over the SPEC §4.1 150 ms dit
        // ceiling, so unimodal init must assume dahs, not the pre-fix
        // default of dits (which decoded "TTTTT" as "5").
        let env = rect_envelope("TTTTT", 24);
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        assert_eq!(events_to_text(&events), "TTTTT");
    }
```

- [ ] **Step 6: Run the full manta-decode test suite**

Run: `cargo test -p manta-decode`
Expected: PASS, including `all_dah_opener_decodes_correctly`.

- [ ] **Step 7: Verify V1 is unaffected**

Run: `cargo test -p manta-cli --test golden_v1`
Expected: PASS (V1 opens with "CQ CQ DE W1AW..." — a mixed opener, not homogeneous — this fix's unimodal branch never even triggers for it, but the ratio constraints and re-anchor paths are shared code, so a regression here would be a real signal).

- [ ] **Step 8: Commit**

```bash
git add crates/manta-decode/src/timing.rs crates/manta-decode/src/decoder.rs
git commit -m "fix(decode): all-dah opener uses absolute-ms prior, not always-assume-dits (pinned decision 20)"
```

---

### Task 3: Hilbert transformer (`manta-dsp::hilbert`)

**Files:**
- Create: `crates/manta-dsp/src/hilbert.rs`
- Modify: `crates/manta-dsp/src/lib.rs`

**Interfaces:**
- Consumes: `crate::proto::{bessel_i0, KAISER_BETA}` (both already `pub`/`pub(crate)` in `proto.rs`, visible crate-wide).
- Produces: `pub struct HilbertTransformer` with `pub fn new() -> Self`, `pub fn delay(&self) -> usize`, `pub fn process(&mut self, input: &[f32]) -> Vec<Complex32>`. `pub const HILBERT_TAPS: usize = 129`. `pub fn design_hilbert_fir() -> Vec<f32>`. Used by Task 4 (`AudioIqSource`) and Task 9 (V4/V5 Watterson vectors).

- [ ] **Step 1: Write the failing tests**

Create `crates/manta-dsp/src/hilbert.rs`:

```rust
//! Real-to-analytic (Hilbert) conversion: odd-length windowed-sinc FIR,
//! Kaiser-windowed identically to the PFB prototype (proto.rs). Used both
//! for live audio input (manta-input::AudioIqSource) and offline
//! Watterson vector rendering (manta-testkit). Design doc §3.

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

/// Hilbert FIR length (odd). 129 taps gives a well-behaved passband from a
/// few hundred Hz to several kHz at 48 kHz -- comfortably covers rig audio
/// and the CW tone offsets M1 uses.
pub const HILBERT_TAPS: usize = 129;

/// Design the length-HILBERT_TAPS windowed-sinc Hilbert FIR:
/// h[n] = 0 for (n - center) even, 2 / (pi * (n - center)) for odd,
/// Kaiser-windowed with the PFB prototype's beta (proto.rs).
pub fn design_hilbert_fir() -> Vec<f32> {
    let len = HILBERT_TAPS;
    let center = (len - 1) as f64 / 2.0; // integer-valued since len is odd
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h = vec![0.0f64; len];
    for (i, tap) in h.iter_mut().enumerate() {
        let k = i as f64 - center;
        let ideal = if (k as i64) % 2 == 0 {
            0.0
        } else {
            2.0 / (std::f64::consts::PI * k)
        };
        let t = 2.0 * i as f64 / (len - 1) as f64 - 1.0;
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = ideal * w;
    }
    h.into_iter().map(|v| v as f32).collect()
}

/// Streaming FIR Hilbert transformer: incrementally converts real samples
/// to an analytic (I = delayed real, Q = Hilbert-filtered) signal. Causal,
/// fixed group delay of (HILBERT_TAPS-1)/2 samples, callable across
/// multiple `process` calls with persistent history (design doc §3).
pub struct HilbertTransformer {
    taps: Vec<f32>,
    /// Ring of the last HILBERT_TAPS real input samples, oldest first.
    hist: std::collections::VecDeque<f32>,
}

impl Default for HilbertTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl HilbertTransformer {
    pub fn new() -> Self {
        HilbertTransformer {
            taps: design_hilbert_fir(),
            hist: std::collections::VecDeque::from(vec![0.0f32; HILBERT_TAPS]),
        }
    }

    /// Fixed causal group delay, in samples: (HILBERT_TAPS - 1) / 2.
    pub fn delay(&self) -> usize {
        (HILBERT_TAPS - 1) / 2
    }

    /// Convert one chunk of real samples to analytic Complex32 samples, one
    /// output per input, using a persistent history window across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<Complex32> {
        let center = self.delay();
        let mut out = Vec::with_capacity(input.len());
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            // Sequential f64 accumulation (SPEC §6.4 determinism convention).
            let mut acc = 0.0f64;
            for (i, &h) in self.taps.iter().enumerate() {
                acc += h as f64 * self.hist[i] as f64;
            }
            let re = self.hist[center];
            out.push(Complex32::new(re, acc as f32));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fir_is_odd_length_and_zero_at_even_offsets() {
        let h = design_hilbert_fir();
        assert_eq!(h.len(), HILBERT_TAPS);
        let center = (HILBERT_TAPS - 1) / 2;
        assert_eq!(h[center], 0.0, "center tap (k=0) must be exactly zero");
        assert_eq!(h[center + 2], 0.0, "k=+2 (even) must be exactly zero");
        assert_eq!(h[center - 2], 0.0, "k=-2 (even) must be exactly zero");
        assert!(h[center + 1] != 0.0, "k=+1 (odd) must be nonzero");
    }

    #[test]
    fn fir_is_antisymmetric() {
        // Ideal Hilbert kernel h[n] = 2/(pi*n) is odd: h[center+k] = -h[center-k].
        let h = design_hilbert_fir();
        let center = (HILBERT_TAPS - 1) / 2;
        for k in 1..center {
            assert!(
                (h[center + k] + h[center - k]).abs() < 1e-6,
                "k={k}: h+={} h-={}",
                h[center + k],
                h[center - k]
            );
        }
    }

    #[test]
    fn real_branch_is_delay_matched() {
        let mut h = HilbertTransformer::new();
        let x: Vec<f32> = (0..500).map(|i| (i as f32 * 0.01).sin()).collect();
        let y = h.process(&x);
        let delay = h.delay();
        for i in delay..x.len() {
            assert_eq!(y[i].re, x[i - delay], "real branch mismatch at {i}");
        }
    }

    #[test]
    fn analytic_signal_of_positive_tone_rotates_forward() {
        let fs = 48_000.0f64;
        let f = 1_000.0f64; // well within the Hilbert passband
        let n = 4096;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs).cos() as f32)
            .collect();
        let mut h = HilbertTransformer::new();
        let y = h.process(&x);
        let delay = h.delay();
        let skip = 2 * delay; // let the filter's transient settle at both ends
        for i in skip..(n - skip) {
            assert!(
                (y[i].norm() - 1.0).abs() < 0.05,
                "i={i} norm={}",
                y[i].norm()
            );
        }
        let expect_dphi = 2.0 * std::f64::consts::PI * f / fs;
        let i = n / 2;
        let dphi = (y[i + 1].im.atan2(y[i + 1].re) - y[i].im.atan2(y[i].re)) as f64;
        let dphi = dphi.rem_euclid(std::f64::consts::TAU);
        assert!(
            (dphi - expect_dphi).abs() < 0.01,
            "dphi={dphi} expect={expect_dphi}"
        );
    }

    #[test]
    fn streams_across_multiple_process_calls() {
        // Splitting one input into chunks must produce byte-identical
        // output to processing it in one call -- the history state persists.
        let fs = 48_000.0f64;
        let f = 700.0f64;
        let n = 2000;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs).cos() as f32)
            .collect();
        let whole = HilbertTransformer::new().process(&x);
        let mut chunked = HilbertTransformer::new();
        let mut out = Vec::new();
        for chunk in x.chunks(137) {
            out.extend(chunked.process(chunk));
        }
        assert_eq!(whole, out);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-dsp/src/lib.rs`, add:

```rust
pub mod hilbert;
```

(alongside the existing `pub mod freqest; pub mod proto; pub mod single;` — check the exact existing line and add `hilbert` to it or as its own line, matching the file's current style.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p manta-dsp hilbert:: 2>&1 | head -20`
Expected: FAIL to compile (module doesn't exist yet) — this is expected; Step 1 already wrote the full implementation inline with the tests (Hilbert transformer design is not separable from its test-driving math the way a simple function is), so compiling should immediately get you to Step 4.

- [ ] **Step 4: Build and run tests**

Run: `cargo test -p manta-dsp hilbert:: -- --nocapture`
Expected: PASS on all five tests. If `analytic_signal_of_positive_tone_rotates_forward` fails on the sign of `dphi`, the FIR sign convention is inverted (h[n] should be `2/(π·n)`, not `-2/(π·n)`) — fix the sign in `design_hilbert_fir` and re-run.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-dsp/src/hilbert.rs crates/manta-dsp/src/lib.rs
git commit -m "feat(dsp): FIR Hilbert transformer for real-to-analytic conversion"
```

---

### Task 4: `AudioIqSource` (manta-input)

**Files:**
- Create: `crates/manta-input/src/audio.rs`
- Modify: `crates/manta-input/src/lib.rs`

**Interfaces:**
- Consumes: `manta_dsp::hilbert::HilbertTransformer` (Task 3); `coppa_audio::{AudioSource, CpalSource, ResamplingSource, WavSource, find_input_device_by_name}` (Task 1); `crate::IqSource` (existing trait, `manta-input/src/lib.rs`).
- Produces: `pub struct AudioIqSource`, `pub const TARGET_RATE_HZ: u32 = 48_000`, `impl AudioIqSource { pub fn new(src: Box<dyn coppa_audio::AudioSource>) -> Result<Self>; pub fn from_device(name: Option<&str>) -> Result<Self>; pub fn from_wav_file(path: &Path) -> Result<Self> }`, `impl IqSource for AudioIqSource`. Used by Task 5 (`manta-engine::listen`) and Task 6 (CLI).

- [ ] **Step 1: Write the failing tests**

Create `crates/manta-input/src/audio.rs`:

```rust
//! Live/replayed real-audio IQ source: coppa-audio AudioSource -> Hilbert
//! transformer -> Complex32, matching IqSource. ARCHITECTURE §3, design
//! doc §2.

use crate::IqSource;
use anyhow::{anyhow, Context, Result};
use coppa_audio::{AudioSource, ResamplingSource};
use num_complex::Complex32;
use manta_dsp::hilbert::HilbertTransformer;
use std::path::Path;

/// Fixed target sample rate for M1 audio decode: 48000 / 93.75 = 512 (a
/// power of two), the constraint SingleChannelExtractor::new requires.
pub const TARGET_RATE_HZ: u32 = 48_000;

/// A real audio source (device or file) converted to analytic Complex32,
/// implementing IqSource. ARCHITECTURE §3 "Audio passband" input.
pub struct AudioIqSource {
    src: Box<dyn AudioSource>,
    hilbert: HilbertTransformer,
}

impl AudioIqSource {
    /// Wrap an already-started AudioSource at TARGET_RATE_HZ.
    pub fn new(src: Box<dyn AudioSource>) -> Result<Self> {
        if src.sample_rate() != TARGET_RATE_HZ {
            return Err(anyhow!(
                "AudioIqSource requires {TARGET_RATE_HZ} Hz, got {}",
                src.sample_rate()
            ));
        }
        Ok(AudioIqSource {
            src,
            hilbert: HilbertTransformer::new(),
        })
    }

    /// Open the named input device (default device if `None`), resampling
    /// to TARGET_RATE_HZ if the device's native rate differs.
    pub fn from_device(name: Option<&str>) -> Result<Self> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let device = match name {
            Some(n) => coppa_audio::find_input_device_by_name(n)
                .ok_or_else(|| anyhow!("no input device matching {n:?}"))?,
            None => cpal::default_host()
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };
        let native_rate = device
            .default_input_config()
            .context("query device default input config")?
            .sample_rate()
            .0;
        let mut cpal_src = coppa_audio::CpalSource::from_device(device, native_rate, 8192)?;
        cpal_src.start()?;
        let boxed: Box<dyn AudioSource> = if native_rate == TARGET_RATE_HZ {
            Box::new(cpal_src)
        } else {
            Box::new(ResamplingSource::new(cpal_src, TARGET_RATE_HZ)?)
        };
        AudioIqSource::new(boxed)
    }

    /// Open a WAV file, replayed as an audio source (soak harness / `listen
    /// --source`). Resampled to TARGET_RATE_HZ if needed.
    pub fn from_wav_file(path: &Path) -> Result<Self> {
        let wav_src = coppa_audio::WavSource::open(path)?;
        let native_rate = wav_src.sample_rate();
        let boxed: Box<dyn AudioSource> = if native_rate == TARGET_RATE_HZ {
            Box::new(wav_src)
        } else {
            Box::new(ResamplingSource::new(wav_src, TARGET_RATE_HZ)?)
        };
        AudioIqSource::new(boxed)
    }
}

impl IqSource for AudioIqSource {
    fn sample_rate(&self) -> f64 {
        TARGET_RATE_HZ as f64
    }

    fn center_freq_hz(&self) -> f64 {
        0.0 // audio has no RF reference; offset-only reporting (design doc §2)
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let got = self.src.read(&mut real)?;
        if got == 0 {
            return Ok(0);
        }
        let analytic = self.hilbert.process(&real[..got]);
        buf[..got].copy_from_slice(&analytic);
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_real_samples_to_analytic_iq() {
        let fs = TARGET_RATE_HZ;
        let f = 1_000.0;
        let samples: Vec<f32> = (0..4000)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs as f64).cos() as f32)
            .collect();
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(samples, fs));
        let mut aiq = AudioIqSource::new(src).unwrap();
        assert_eq!(aiq.sample_rate(), fs as f64);
        assert_eq!(aiq.center_freq_hz(), 0.0);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4000];
        let n = aiq.read(&mut buf).unwrap();
        assert!(n > 0);
        // Well past the Hilbert filter's transient, magnitude should be ~unit.
        assert!(
            (buf[2000].norm() - 1.0).abs() < 0.1,
            "norm={}",
            buf[2000].norm()
        );
    }

    #[test]
    fn rejects_mismatched_sample_rate() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 10], 44_100));
        assert!(AudioIqSource::new(src).is_err());
    }

    #[test]
    fn reports_eof_as_zero_read() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 5], TARGET_RATE_HZ));
        let mut aiq = AudioIqSource::new(src).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(aiq.read(&mut buf).unwrap(), 5);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-input/src/lib.rs`, add near the top (after the existing module doc comment):

```rust
pub mod audio;
pub use audio::{AudioIqSource, TARGET_RATE_HZ};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p manta-input audio:: -- --nocapture`
Expected: PASS on all three tests.

- [ ] **Step 4: Run the full manta-input suite**

Run: `cargo test -p manta-input`
Expected: PASS, including the existing WAV-IQ tests (unaffected by this change).

- [ ] **Step 5: Commit**

```bash
git add crates/manta-input/src/audio.rs crates/manta-input/src/lib.rs
git commit -m "feat(input): AudioIqSource - real audio to analytic Complex32 via coppa-audio + Hilbert"
```

---

### Task 5: Streaming engine (`manta-engine::listen`) + CLI `listen` subcommand

**Files:**
- Create: `crates/manta-engine/src/listen.rs`
- Modify: `crates/manta-engine/src/lib.rs`
- Modify: `crates/manta-cli/src/main.rs`

**Interfaces:**
- Consumes: `manta_input::{AudioIqSource, IqSource}` (Task 4); `manta_dsp::{freqest::estimate_peak_hz, single::SingleChannelExtractor}` (existing); `manta_decode::decoder::TrackDecoder`, `manta_decode::events::DecoderEvent` (existing); `crate::PipelineConfig` (existing, `manta-engine/src/lib.rs`).
- Produces: `pub fn listen(src: AudioIqSource, cfg: &PipelineConfig, stop: Arc<AtomicBool>, on_event: impl FnMut(&DecoderEvent)) -> Result<()>`, re-exported as `manta_engine::listen`. Used by Task 6 (CLI `listen`) and Task 10 (`soak`).

- [ ] **Step 1: Write the failing integration test**

Create `crates/manta-engine/tests/listen_audio.rs`:

```rust
//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use num_complex::Complex32;
use manta_engine::{listen, PipelineConfig};
use manta_input::AudioIqSource;
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[test]
fn listen_decodes_a_clean_real_audio_signal() {
    let fs = 48_000.0;
    let tone_hz = 700.0; // typical CW sidetone offset, well inside audio passband
    let spec = KeyerSpec::new(20.0);
    let (env, keyed_text) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();

    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * tone_hz / fs;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }

    let src = AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(
        real, 48_000,
    )))
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let text = Arc::new(Mutex::new(String::new()));
    let text_clone = text.clone();
    listen(src, &PipelineConfig::default(), stop, move |ev| {
        if let manta_decode::events::DecoderEvent::CharDecoded { glyph, .. } = ev {
            if let Some(c) = glyph.text_char() {
                text_clone.lock().unwrap().push(c);
            }
        }
        if matches!(
            ev,
            manta_decode::events::DecoderEvent::WordBoundary { .. }
        ) {
            text_clone.lock().unwrap().push(' ');
        }
    })
    .unwrap();

    let decoded = text.lock().unwrap().trim().to_string();
    assert!(
        decoded.contains("W1AW"),
        "expected W1AW in decoded text, got {decoded:?} (keyed: {keyed_text:?})"
    );
}
```

Add `[dev-dependencies]` entries to `crates/manta-engine/Cargo.toml` if not already present: `num-complex` is already a normal dependency; add:

```toml
coppa-audio = { workspace = true }
```

to `[dev-dependencies]` (this test only, not the crate's public API).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p manta-engine --test listen_audio 2>&1 | head -20`
Expected: FAIL to compile — `manta_engine::listen` doesn't exist yet.

- [ ] **Step 3: Implement `manta-engine::listen`**

Create `crates/manta-engine/src/listen.rs`:

```rust
//! M1 streaming pipeline: live/replayed audio -> single channel -> decoder,
//! run continuously until Ctrl-C or EOF. No actor/ring-thread split -- M1
//! has exactly one track; see design doc §4.

use crate::PipelineConfig;
use anyhow::{Context, Result};
use num_complex::Complex32;
use manta_decode::decoder::TrackDecoder;
use manta_decode::events::DecoderEvent;
use manta_dsp::freqest::estimate_peak_hz;
use manta_dsp::single::SingleChannelExtractor;
use manta_input::{AudioIqSource, IqSource};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered to estimate the initial channel offset before
/// the extractor is built -- a fixed startup calibration, not re-estimated
/// afterward (M1 has no PFB/track manager to re-tune mid-stream).
const CALIBRATION_SECONDS: f64 = 2.0;

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
pub fn listen(
    mut src: AudioIqSource,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
) -> Result<()> {
    let fs = src.sample_rate();

    let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
    let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
    let mut filled = 0;
    while filled < calib_n {
        let n = src.read(&mut calib[filled..])?;
        if n == 0 {
            anyhow::bail!("audio source ended during startup calibration");
        }
        filled += n;
    }
    let offset_hz = estimate_peak_hz(&calib, fs)
        .context("no signal found during startup calibration")?;

    let mut extractor =
        SingleChannelExtractor::new(fs, offset_hz).map_err(|e| anyhow::anyhow!(e))?;
    let hop = extractor.hop() as u64;
    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(offset_hz);

    // Same lead-in fix as the M0 batch path (extractor group-delay blind
    // zone), applied once at stream start instead of per-file: prime the
    // extractor with one filter length of zero IQ before real audio, and
    // feed every resulting output (do not skip -- see M0 pinned decision 19).
    let pad_samples = extractor.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    let mut m: u64 = 0;
    for y in extractor.process(&padding) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(y.norm(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }
    // Feed the calibration window too -- it was already consumed from the
    // source and must not be discarded.
    for y in extractor.process(&calib) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(y.norm(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break; // EOF (file replay only; live sources block instead)
        }
        for y in extractor.process(&chunk[..n]) {
            let sample_ts = m.saturating_sub(pad_hops) * hop;
            for ev in decoder.push_envelope(y.norm(), sample_ts) {
                on_event(&ev);
            }
            m += 1;
        }
    }
    for ev in decoder.finish() {
        on_event(&ev);
    }
    Ok(())
}
```

- [ ] **Step 4: Register the module**

In `crates/manta-engine/src/lib.rs`, add near the top:

```rust
pub mod listen;
pub use listen::listen;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p manta-engine --test listen_audio -- --nocapture`
Expected: PASS. If the decoded text doesn't contain "W1AW", check: (a) the tone frequency (700 Hz) is within `estimate_peak_hz`'s and the extractor's valid range for `fs=48000`; (b) `key_text_loop`'s 15-second duration is enough for at least one full "CQ CQ DE W1AW W1AW K" cycle at 20 WPM plus the calibration + lead-in overhead to still leave real signal.

- [ ] **Step 6: Add the CLI `listen` subcommand**

Read `crates/manta-cli/src/main.rs` in full before editing (already read during planning — the `Command` enum and `main()` match are small). Add to the `Command` enum:

```rust
    /// Decode a live off-air CW signal continuously from real audio.
    Listen {
        /// Input device name substring (default input device if omitted).
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        /// Replay a WAV file instead of a live device (paced by its own
        /// sample rate via AudioIqSource; used for demos and testing).
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        /// Emit DecoderEvents as JSON Lines instead of plain text.
        #[arg(long)]
        json: bool,
    },
```

Add to `main()`'s match, alongside `Command::Decode`/`Command::Gen`:

```rust
        Command::Listen {
            device,
            source,
            json,
        } => {
            let src = match source {
                Some(path) => manta_input::AudioIqSource::from_wav_file(&path)?,
                None => manta_input::AudioIqSource::from_device(device.as_deref())?,
            };
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            manta_engine::listen(src, &PipelineConfig::default(), stop, |ev| {
                if json {
                    println!("{}", serde_json::to_string(ev).unwrap());
                    return;
                }
                use manta_decode::events::DecoderEvent;
                use std::io::Write as _;
                match ev {
                    DecoderEvent::CharDecoded { glyph, .. } => {
                        if let Some(c) = glyph.text_char() {
                            print!("{c}");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    DecoderEvent::WordBoundary { .. } => {
                        print!(" ");
                        let _ = std::io::stdout().flush();
                    }
                    _ => {}
                }
            })?;
        }
```

- [ ] **Step 7: Build the CLI**

Run: `cargo build -p manta-cli 2>&1 | tail -30`
Expected: clean build. If `ctrlc::set_handler` returns a `Result` whose error type doesn't satisfy `?`'s conversion to `anyhow::Error`, wrap it: `ctrlc::set_handler(...).map_err(|e| anyhow::anyhow!("{e}"))?;`.

- [ ] **Step 8: Manual smoke test (not automated -- confirms the CLI wiring, not decode accuracy)**

Run: `cargo run -p manta-cli -- listen --source crates/manta-testkit/fixtures/v1.wav` (generate `v1.wav` first via `cargo run -p manta-cli -- gen v1 --out crates/manta-testkit/fixtures` if it doesn't exist — note V1 is a complex IQ WAV, not real audio, so this specific smoke test is expected to either error cleanly (2-channel WAV rejected by `coppa_audio::WavSource`, which reads channel 0 only and silently treats channel 1 as absent) or decode garbage; its purpose is only to confirm the binary runs without panicking end-to-end, not to validate output).
Expected: process starts, reads to EOF, exits 0, no panic.

- [ ] **Step 9: Commit**

```bash
git add crates/manta-engine/src/listen.rs crates/manta-engine/src/lib.rs crates/manta-engine/tests/listen_audio.rs crates/manta-engine/Cargo.toml crates/manta-cli/src/main.rs
git commit -m "feat(engine,cli): streaming listen pipeline + manta listen subcommand"
```

---

### Task 6: Determinism test — chunked streaming vs. whole-buffer batch decode

**Files:**
- Create: `crates/manta-engine/tests/chunking_determinism.rs`

**Interfaces:**
- Consumes: `manta_dsp::single::SingleChannelExtractor`, `manta_decode::decoder::TrackDecoder` (existing, unchanged); `manta_testkit::vectors::v1` (existing).
- Produces: nothing new — this test proves a property design doc §4 asserts ("the incremental extractor/decoder API was already built streaming-capable"), entirely within the existing complex-IQ domain (V1's fixture), independent of Tasks 3-5's real-audio path.

- [ ] **Step 1: Write the test**

Create `crates/manta-engine/tests/chunking_determinism.rs`:

```rust
//! Proves the M1 streaming design's core claim (design doc §4): feeding
//! SingleChannelExtractor/TrackDecoder in small chunks produces
//! byte-identical output to M0's single whole-buffer call. This is what
//! lets `listen`'s per-chunk loop reuse the M0 decode chain unchanged.

use num_complex::Complex32;
use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::freqest::estimate_peak_hz;
use manta_dsp::single::SingleChannelExtractor;
use manta_testkit::vectors::{render, v1};

fn decode_all_at_once(iq: &[Complex32], fs: f64, offset_hz: f64) -> Vec<DecoderEvent> {
    let mut extractor = SingleChannelExtractor::new(fs, offset_hz).unwrap();
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for (m, y) in extractor.process(iq).into_iter().enumerate() {
        events.extend(decoder.push_envelope(y.norm(), m as u64 * extractor.hop() as u64));
    }
    events.extend(decoder.finish());
    events
}

fn decode_in_chunks(
    iq: &[Complex32],
    fs: f64,
    offset_hz: f64,
    chunk_size: usize,
) -> Vec<DecoderEvent> {
    let mut extractor = SingleChannelExtractor::new(fs, offset_hz).unwrap();
    let hop = extractor.hop() as u64;
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    let mut m: u64 = 0;
    for chunk in iq.chunks(chunk_size) {
        for y in extractor.process(chunk) {
            events.extend(decoder.push_envelope(y.norm(), m * hop));
            m += 1;
        }
    }
    events.extend(decoder.finish());
    events
}

#[test]
fn chunked_feeding_matches_whole_buffer_feeding() {
    let spec = v1();
    let rendered = render(&spec).unwrap();
    let offset_hz = spec.signals[0].offset_hz;

    let whole = decode_all_at_once(&rendered.samples, spec.fs, offset_hz);
    for &chunk_size in &[97usize, 1_024, 8_192, 100_000] {
        let chunked = decode_in_chunks(&rendered.samples, spec.fs, offset_hz, chunk_size);
        assert_eq!(
            whole, chunked,
            "chunk_size={chunk_size} produced different events than whole-buffer decode"
        );
    }
}
```

(`estimate_peak_hz` is imported but unused here since V1's offset is known ground truth — remove the unused import if `cargo build` warns, or use it to independently re-derive `offset_hz` from `rendered.samples` for extra rigor: `let offset_hz = estimate_peak_hz(&rendered.samples, spec.fs).unwrap();` — prefer this version, since it doesn't rely on the vector's declared offset matching what the frequency estimator actually finds.)

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `cargo test -p manta-engine --test chunking_determinism -- --nocapture`
Expected: PASS immediately — this task adds no new production code, only a test proving an existing property. If it FAILS, that's a real bug in `SingleChannelExtractor`'s or `TrackDecoder`'s incremental-call handling (both were designed to support this, per their existing internal `buf`/`read`/`n_in` state, but this is the first test to actually exercise multiple `process()` calls against a real golden vector rather than one whole-buffer call) — stop and investigate before proceeding; do not weaken the assertion.

- [ ] **Step 3: Commit**

```bash
git add crates/manta-engine/tests/chunking_determinism.rs
git commit -m "test(engine): prove chunked streaming feeding matches whole-buffer batch decode"
```

---

### Task 7: V2/V3 golden vectors (AWGN + jitter, no new machinery)

**Files:**
- Modify: `crates/manta-testkit/src/vectors.rs`
- Modify: `crates/manta-cli/src/main.rs`

**Interfaces:**
- Consumes: existing `VectorSpec`, `SignalSpec`, `render`, `write_fixture_set` (unchanged by this task).
- Produces: `pub fn v2() -> VectorSpec`, `pub fn v3() -> VectorSpec` in `manta_testkit::vectors`.

- [ ] **Step 1: Add v2/v3 to vectors.rs**

In `crates/manta-testkit/src/vectors.rs`, add `use crate::keyer::Jitter;` to the imports, then add after `v1()`:

```rust
/// SPEC §7 V2 "fast-35": 35 WPM, +15 dB, JA1ABC, AWGN + 8% jitter.
pub fn v2() -> VectorSpec {
    VectorSpec {
        name: "v2",
        fs: 96_000.0,
        duration_s: 90.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5632, // "SKIMV2"
        signals: vec![SignalSpec {
            text: "CQ CQ DE JA1ABC JA1ABC K".into(),
            loop_text: true,
            wpm: 35.0,
            offset_hz: -8_200.0,
            snr_2500_db: 15.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: 0x5632,
            }),
        }],
    }
}

/// SPEC §7 V3 "slow-weak": 12 WPM, +6 dB, VK9DX, AWGN + 8% jitter.
pub fn v3() -> VectorSpec {
    VectorSpec {
        name: "v3",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5633, // "SKIMV3"
        signals: vec![SignalSpec {
            text: "CQ CQ DE VK9DX VK9DX K".into(),
            loop_text: true,
            wpm: 12.0,
            offset_hz: 5_600.0,
            snr_2500_db: 6.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: 0x5633,
            }),
        }],
    }
}
```

- [ ] **Step 2: Add unit tests mirroring v1's**

In `vectors.rs`'s `mod tests`, add:

```rust
    #[test]
    fn v2_spec_matches_spec_table() {
        let v = v2();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 35.0);
        assert_eq!(s.snr_2500_db, 15.0);
        assert!(s.jitter.is_some());
        assert_eq!(s.text, "CQ CQ DE JA1ABC JA1ABC K");
    }

    #[test]
    fn v3_spec_matches_spec_table() {
        let v = v3();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 12.0);
        assert_eq!(s.snr_2500_db, 6.0);
        assert!(s.jitter.is_some());
        assert_eq!(s.text, "CQ CQ DE VK9DX VK9DX K");
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p manta-testkit vectors:: -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Wire into CLI `gen` and add golden-decode test**

In `crates/manta-cli/src/main.rs`, extend the `Gen` match arm:

```rust
                "v1" => manta_testkit::vectors::v1(),
                "v2" => manta_testkit::vectors::v2(),
                "v3" => manta_testkit::vectors::v3(),
```

(V4-V6 are added by Tasks 8-9 to the same match arm and will widen the `bail!` message; leave it as `bail!("unknown vector {other:?} (available: v1-v3)")` for now.)

Create `crates/manta-cli/tests/golden_v2_v3.rs`, following `golden_v1.rs`'s exact pattern (spawn the built `manta` binary via `Command::new(env!("CARGO_BIN_EXE_manta"))`, `decode --json` the fixture, compare via `manta_testkit::cer::cer`):

```rust
//! SPEC §7 V2/V3 golden gates.
//! V2 "fast-35": char accuracy >= 99 %; WPM reported 35 +/- 2.
//! V3 "slow-weak": char accuracy >= 95 %.

use std::process::Command;

fn decode_report(spec: &manta_testkit::vectors::VectorSpec) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

#[test]
fn v2_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v2();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.01,
        "V2 char accuracy must be >= 99 % (CER <= 0.01), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 35.0).abs() < 2.0, "wpm {wpm}");
}

#[test]
fn v3_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v3();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V3 char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}
```

- [ ] **Step 5: Run the golden tests**

Run: `cargo test -p manta-cli golden 2>&1 | tail -40`
Expected: PASS. If accuracy falls short, this is real signal about the decode chain at 35 WPM/12 WPM — do not lower the threshold to make it pass; investigate (likely candidates: `DIT_CLAMP_MS` bounds at the speed extremes, or `MAX_SECONDS`/`FFT_SIZE` in `freqest.rs` not resolving the frequency cleanly at 90s/12WPM's slower keying rate).

- [ ] **Step 6: Commit**

```bash
git add crates/manta-testkit/src/vectors.rs crates/manta-cli/src/main.rs crates/manta-cli/tests/
git commit -m "test(testkit): V2 (fast-35) and V3 (slow-weak) golden vectors"
```

---

### Task 8: V6 golden vector (sinusoidal QSB envelope)

**Files:**
- Modify: `crates/manta-testkit/src/scene.rs`
- Modify: `crates/manta-testkit/src/vectors.rs`
- Modify: `crates/manta-cli/src/main.rs`

**Interfaces:**
- Produces: `pub struct QsbSine { pub rate_hz: f32 }` added to `scene.rs`; `SignalSpec` gains `pub qsb: Option<QsbSine>`; `pub fn v6() -> VectorSpec` in `vectors.rs`.

- [ ] **Step 1: Add `QsbSine` and the `qsb` field**

In `crates/manta-testkit/src/scene.rs`, add after the `SignalSpec` struct:

```rust
/// Sinusoidal QSB envelope multiplier applied on top of the keyed envelope.
/// SPEC §7 V6: `0.55 + 0.45 * sin(2*pi*rate_hz*t)`.
#[derive(Debug, Clone, Copy)]
pub struct QsbSine {
    pub rate_hz: f32,
}
```

Add `pub qsb: Option<QsbSine>,` as a new field on `SignalSpec`, immediately after `pub jitter: Option<Jitter>,`.

- [ ] **Step 2: Update every existing `SignalSpec` literal**

Every place a `SignalSpec { .. }` is constructed by name (not via `..spec` update syntax) needs `qsb: None,` added:
- `crates/manta-testkit/src/vectors.rs`: `v1()` (and `v2()`/`v3()` from Task 7).
- `crates/manta-testkit/src/scene.rs`'s `mod tests`: `achieved_snr_matches_request`, `scene_is_deterministic`.

Add `qsb: None,` to each. (Task 9 will similarly need `watterson: None,` added to all of these plus this task's own `v6()` and Task 7's v2/v3 — do that in Task 9, not here, to keep this task's diff focused.)

- [ ] **Step 3: Apply the QSB multiplier in `render_scene`**

In `render_scene`'s per-signal sample loop, change:

```rust
        for (i, out) in acc.iter_mut().enumerate() {
            let e = env.get(i).copied().unwrap_or(0.0) * amp;
```

to:

```rust
        for (i, out) in acc.iter_mut().enumerate() {
            let mut e = env.get(i).copied().unwrap_or(0.0) * amp;
            if let Some(q) = sig.qsb {
                let t = i as f64 / fs;
                let mul = 0.55 + 0.45 * (std::f64::consts::TAU * q.rate_hz as f64 * t).sin();
                e *= mul as f32;
            }
```

(the rest of the loop body — the `if e != 0.0 { ... }` NCO placement — is unchanged; only the `e` computation gains the QSB multiply, and the `let e` binding must become `let mut e` and the multiply inserted before the existing `if e != 0.0` check.)

- [ ] **Step 4: Add v6() to vectors.rs**

```rust
/// SPEC §7 V6 "qsb-sine": 20 WPM, K5ZZZ, AWGN, sinusoidal envelope QSB.
pub fn v6() -> VectorSpec {
    VectorSpec {
        name: "v6",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5636, // "SKIMV6"
        signals: vec![SignalSpec {
            text: "CQ CQ DE K5ZZZ K5ZZZ K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: -15_000.0,
            snr_2500_db: 20.0, // peak SNR; QSB brings the trough toward ~0 dB
            jitter: None,
            qsb: Some(QsbSine { rate_hz: 0.2 }),
        }],
    }
}
```

Add `use crate::scene::QsbSine;` to `vectors.rs`'s imports (it currently imports `SignalSpec` from `crate::scene` — add `QsbSine` to that same `use` line).

- [ ] **Step 5: Unit test**

In `vectors.rs`'s `mod tests`:

```rust
    #[test]
    fn v6_spec_matches_spec_table() {
        let v = v6();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 20.0);
        let qsb = s.qsb.expect("V6 must carry a QsbSine spec");
        assert_eq!(qsb.rate_hz, 0.2);
    }
```

Add a `scene.rs`-level test proving the multiplier actually varies amplitude over time:

```rust
    #[test]
    fn qsb_sine_modulates_envelope_amplitude() {
        let fs = 96_000.0;
        let sig = SignalSpec {
            text: "E".into(), // one dit, looped -- near-continuous keying
            loop_text: true,
            wpm: 20.0,
            offset_hz: 1_000.0,
            snr_2500_db: 30.0,
            jitter: None,
            qsb: Some(QsbSine { rate_hz: 0.2 }),
        };
        // One full QSB period at 0.2 Hz is 5 s; render 5 s and confirm the
        // peak envelope magnitude varies by roughly the expected 0.55+-0.45 range.
        let (samples, _) = render_scene(std::slice::from_ref(&sig), fs, 5.0, None).unwrap();
        let peak = samples.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
        let trough_window: Vec<f32> = samples
            .iter()
            .skip(samples.len() / 2 - 100)
            .take(200)
            .map(|c| c.norm())
            .collect();
        let trough_max = trough_window.iter().copied().fold(0.0f32, f32::max);
        assert!(
            trough_max < peak * 0.5,
            "expected a QSB trough well below peak: peak={peak} trough_max={trough_max}"
        );
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p manta-testkit scene:: vectors:: -- --nocapture`
Expected: PASS. `qsb_sine_modulates_envelope_amplitude`'s trough-window index (`samples.len()/2`) is a rough guess at where the 0.2 Hz sine dips — if it doesn't land near a trough, adjust the window index using the known sine phase (`sin(2π·0.2·t)` is at its minimum at `t = 2.5s` within a 5s render, i.e. sample index `2.5*fs`) rather than loosening the assertion.

- [ ] **Step 7: Wire v6 into CLI `gen`, add golden test**

In `crates/manta-cli/src/main.rs`'s `Gen` match arm, add `"v6" => manta_testkit::vectors::v6(),` and widen the `bail!` message to `"available: v1-v3, v6"`.

Add to `crates/manta-cli/tests/golden_v2_v3.rs` (or a same-pattern new file `golden_v6.rs` — either is fine, keep the `decode_report` helper accessible to whichever file uses it, duplicating it if using a separate file):

```rust
#[test]
fn v6_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v6();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "V6 char accuracy must be >= 90 % (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // "Track survives" (ROADMAP): at least one CharDecoded event must land
    // in the render's second half, proving the decoder didn't silently
    // stop producing output during the QSB trough (no explicit
    // track-closed event exists yet at M0/M1 to assert against directly).
    let events = report["events"].as_array().unwrap();
    // sample_ts is in raw input samples at manifest.fs (SPEC §1.1's
    // extractor timing, NOT the 375 Hz channel-output rate).
    let half_ts = (manifest.duration_s / 2.0 * manifest.fs) as u64;
    let survives_past_half = events.iter().any(|ev| {
        ev["event"].as_str() == Some("CharDecoded")
            && ev["sample_ts"].as_u64().unwrap_or(0) > half_ts
    });
    assert!(survives_past_half, "no CharDecoded event past the render midpoint");
}
```

- [ ] **Step 8: Run and commit**

Run: `cargo test -p manta-cli golden 2>&1 | tail -40`
Expected: PASS.

```bash
git add crates/manta-testkit/src/scene.rs crates/manta-testkit/src/vectors.rs crates/manta-cli/src/main.rs crates/manta-cli/tests/
git commit -m "test(testkit): V6 (qsb-sine) golden vector"
```

---

### Task 9: V4/V5 golden vectors (Watterson fading via coppa's one-shot API + Hilbert)

**Files:**
- Modify: `crates/manta-testkit/src/scene.rs`
- Modify: `crates/manta-testkit/src/vectors.rs`
- Modify: `crates/manta-cli/src/main.rs`

**Interfaces:**
- Consumes: `coppa_channel::watterson::{watterson_preset, WattersonPreset}` (Task 1); `manta_dsp::hilbert::HilbertTransformer` (Task 3).
- Produces: `pub struct WattersonFade { pub preset: WattersonPreset, pub seed: u64 }`; `SignalSpec` gains `pub watterson: Option<WattersonFade>`; `pub fn v4() -> VectorSpec`, `pub fn v5() -> VectorSpec`.

- [ ] **Step 1: Add `WattersonFade` and the `watterson` field**

In `crates/manta-testkit/src/scene.rs`, add:

```rust
/// Watterson HF fading applied to this signal only, via coppa's real,
/// currently-shipped `watterson_preset()` (one-shot, real-domain) -- design
/// doc §6: the streaming `WattersonChannel` SPEC-decode-core.md assumes was
/// never implemented in coppa; vector generation is offline/batch, so the
/// streaming requirement doesn't apply.
#[derive(Debug, Clone, Copy)]
pub struct WattersonFade {
    pub preset: coppa_channel::watterson::WattersonPreset,
    pub seed: u64,
}
```

Add `pub watterson: Option<WattersonFade>,` to `SignalSpec`, after `pub qsb: Option<QsbSine>,`.

- [ ] **Step 2: Update every existing `SignalSpec` literal with `watterson: None,`**

Same set of sites as Task 8 Step 2, plus this task's own `v6()`: `vectors.rs`'s `v1()`, `v2()`, `v3()`, `v6()`, and `scene.rs`'s `achieved_snr_matches_request`, `scene_is_deterministic`, `qsb_sine_modulates_envelope_amplitude`.

- [ ] **Step 3: Add the Watterson code path in `render_scene`**

In `render_scene`, restructure the per-signal loop to branch on `sig.watterson`:

```rust
    for sig in signals {
        let spec = KeyerSpec {
            wpm: sig.wpm,
            rise_ms: 5.0,
            jitter: sig.jitter,
        };
        let (env, text) = if sig.loop_text {
            key_text_loop(&sig.text, &spec, fs, duration_s)?
        } else {
            key_text(&sig.text, &spec, fs)?
        };
        texts.push(text);
        let amp = amplitude_for_snr_2500(sig.snr_2500_db, fs);

        if let Some(fade) = sig.watterson {
            // Real-domain path: build a real passband tone at this
            // signal's offset, fade it with coppa's model, then
            // Hilbert-convert back to complex baseband and add directly.
            let mut real = vec![0.0f32; n];
            let dphi = std::f64::consts::TAU * sig.offset_hz / fs;
            let mut phi = 0.0f64;
            for (i, r) in real.iter_mut().enumerate() {
                let e = env.get(i).copied().unwrap_or(0.0) * amp;
                *r = e * phi.cos() as f32;
                phi += dphi;
                if phi > std::f64::consts::PI {
                    phi -= std::f64::consts::TAU;
                } else if phi < -std::f64::consts::PI {
                    phi += std::f64::consts::TAU;
                }
            }
            let faded = coppa_channel::watterson::watterson_preset(
                &real,
                fs as f32,
                fade.preset,
                fade.seed,
            );
            let analytic = manta_dsp::hilbert::HilbertTransformer::new().process(&faded);
            for (out, a) in acc.iter_mut().zip(analytic.iter()) {
                *out += a;
            }
            continue;
        }

        // AWGN-only path (V1-V3, V6): complex NCO placement.
        let dphi = std::f64::consts::TAU * sig.offset_hz / fs;
        let mut phi = 0.0f64;
        for (i, out) in acc.iter_mut().enumerate() {
            let mut e = env.get(i).copied().unwrap_or(0.0) * amp;
            if let Some(q) = sig.qsb {
                let t = i as f64 / fs;
                let mul = 0.55 + 0.45 * (std::f64::consts::TAU * q.rate_hz as f64 * t).sin();
                e *= mul as f32;
            }
            if e != 0.0 {
                let (s, c) = phi.sin_cos();
                out.re += e * c as f32;
                out.im += e * s as f32;
            }
            phi += dphi;
            if phi > std::f64::consts::PI {
                phi -= std::f64::consts::TAU;
            } else if phi < -std::f64::consts::PI {
                phi += std::f64::consts::TAU;
            }
        }
    }
```

(This is the full replacement for the existing per-signal loop body — the `n` local, `acc` buffer, and the trailing `add_unit_awgn`/`MASTER_SCALE` steps after the `for sig in signals` loop are unchanged.)

- [ ] **Step 4: Add v4()/v5() to vectors.rs**

Add `use coppa_channel::watterson::WattersonPreset;` and `use crate::scene::WattersonFade;` to `vectors.rs`'s imports (extend the existing `use crate::scene::{...}` line).

```rust
/// SPEC §7 V4 "fade-good": 25 WPM, +10 dB, DL1ABC, Watterson CCIR-good.
pub fn v4() -> VectorSpec {
    VectorSpec {
        name: "v4",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5634, // "SKIMV4"
        signals: vec![SignalSpec {
            text: "CQ CQ DE DL1ABC DL1ABC K".into(),
            loop_text: true,
            wpm: 25.0,
            offset_hz: 9_100.0,
            snr_2500_db: 10.0,
            jitter: None,
            qsb: None,
            watterson: Some(WattersonFade {
                preset: WattersonPreset::Good,
                seed: 0x5634,
            }),
        }],
    }
}

/// SPEC §7 V5 "fade-poor": 22 WPM, +3 dB, ZL2XYZ, Watterson CCIR-poor.
pub fn v5() -> VectorSpec {
    VectorSpec {
        name: "v5",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5635, // "SKIMV5"
        signals: vec![SignalSpec {
            text: "CQ CQ DE ZL2XYZ ZL2XYZ K".into(),
            loop_text: true,
            wpm: 22.0,
            offset_hz: -11_300.0,
            snr_2500_db: 3.0,
            jitter: None,
            qsb: None,
            watterson: Some(WattersonFade {
                preset: WattersonPreset::Poor,
                seed: 0x5635,
            }),
        }],
    }
}
```

- [ ] **Step 5: Unit tests**

```rust
    #[test]
    fn v4_spec_matches_spec_table() {
        let v = v4();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 25.0);
        assert_eq!(s.snr_2500_db, 10.0);
        assert!(s.watterson.is_some());
    }

    #[test]
    fn v5_spec_matches_spec_table() {
        let v = v5();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 22.0);
        assert_eq!(s.snr_2500_db, 3.0);
        assert!(s.watterson.is_some());
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p manta-testkit -- --nocapture`
Expected: PASS across the full crate (this touches shared code in `scene.rs`, so run the whole crate, not just `vectors::`).

- [ ] **Step 7: Wire v4/v5 into CLI `gen`, widen the bail message**

```rust
                "v1" => manta_testkit::vectors::v1(),
                "v2" => manta_testkit::vectors::v2(),
                "v3" => manta_testkit::vectors::v3(),
                "v4" => manta_testkit::vectors::v4(),
                "v5" => manta_testkit::vectors::v5(),
                "v6" => manta_testkit::vectors::v6(),
                other => bail!("unknown vector {other:?} (available: v1-v6)"),
```

- [ ] **Step 8: Add golden tests, run, and commit**

Add to `crates/manta-cli/tests/golden_v2_v3.rs` (reusing its `decode_report` helper):

```rust
#[test]
fn v4_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v4();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V4 char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

#[test]
fn v5_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v5();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.20,
        "V5 char accuracy must be >= 80 % (CER <= 0.20), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );

    // Callsign validated within 90 s: find the sample_ts at which "ZL2XYZ"
    // first appears as a contiguous substring of the running decoded text.
    // M1 doesn't have manta-spot's callsign validation yet, so this
    // approximates ROADMAP's "callsign validated within 90 s" gate.
    // sample_ts is in raw input samples at manifest.fs (SPEC §1.1).
    let events = report["events"].as_array().unwrap();
    let mut running = String::new();
    let mut validated_ts: Option<f64> = None;
    for ev in events {
        if ev["event"].as_str() == Some("CharDecoded") {
            if let Some(c) = ev["glyph"]["Char"].as_str() {
                running.push_str(c);
            }
            if validated_ts.is_none() && running.contains("ZL2XYZ") {
                validated_ts = ev["sample_ts"].as_u64().map(|ts| ts as f64);
            }
        }
    }
    let validated_ts = validated_ts.expect("ZL2XYZ never appeared in decoded output");
    assert!(
        validated_ts <= 90.0 * manifest.fs,
        "ZL2XYZ validated at {:.1} s, expected <= 90 s",
        validated_ts / manifest.fs
    );
}
```

Run: `cargo test -p manta-cli golden 2>&1 | tail -60`
Expected: PASS. If V5 (CCIR-poor, +3 dB) falls short of 80%, this is real signal about decode robustness under fading — do not lower the threshold; if it's a rendering bug (e.g. Hilbert delay misalignment corrupting the very start of the message), re-check Step 3's real-domain construction against Task 3's Hilbert transformer tests.

```bash
git add crates/manta-testkit/src/scene.rs crates/manta-testkit/src/vectors.rs crates/manta-cli/src/main.rs crates/manta-cli/tests/
git commit -m "test(testkit): V4 (fade-good) and V5 (fade-poor) golden vectors via coppa watterson_preset + Hilbert"
```

---

### Task 10: Soak harness (`manta-engine::soak`) + CLI `soak` subcommand

**Files:**
- Create: `crates/manta-engine/src/soak.rs`
- Modify: `crates/manta-engine/src/lib.rs`
- Modify: `crates/manta-cli/src/main.rs`
- Create: `crates/manta-cli/tests/soak_ci.rs`

**Interfaces:**
- Consumes: `crate::listen` (Task 5); `crate::PipelineConfig`.
- Produces: `pub struct SoakReport { pub events_emitted: usize, pub rss_growth_bytes: u64, pub panicked: bool }`, `pub fn soak(src: AudioIqSource, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport>`, `pub fn soak_passed(report: &SoakReport) -> bool`.

- [ ] **Step 1: Implement the soak harness**

Create `crates/manta-engine/src/soak.rs`. Note up front: `listen()` returns
`Result<()>`, so `catch_unwind`'s closure returns `Result<()>` and
`result: std::thread::Result<Result<()>>` — `Err(_)` at the outer level is a
genuine panic; `Ok(Err(e))` is `listen()` returning cleanly with an error
(e.g. "no signal found during calibration"), which is NOT a panic and must
be surfaced as an error, not silently folded into `panicked`.

```rust
//! Soak harness: run the listen pipeline for a fixed duration, asserting no
//! panic and bounded memory growth. ROADMAP M1 accept criterion; reused by
//! M2/M3's longer soaks (design doc §7).
//!
//! Deviation from the design doc: input-overrun tracking is NOT
//! implemented. coppa-audio's CpalSource doesn't expose its internal
//! ring's overflow_count() publicly, and file-replay sources (what this
//! harness runs against in CI) have no ring and cannot overrun by
//! construction. Live-hardware overrun observability needs a coppa-audio
//! API addition -- a real upstream ask, not made unilaterally here.

use crate::{listen, PipelineConfig};
use anyhow::Result;
use manta_input::AudioIqSource;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Growth in peak RSS beyond this, after the warm-up window, fails the soak.
const RSS_GROWTH_LIMIT_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB
const WARMUP: Duration = Duration::from_secs(10);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SoakReport {
    pub events_emitted: usize,
    pub rss_growth_bytes: u64,
    pub panicked: bool,
}

fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            raw // macOS reports ru_maxrss in bytes
        } else {
            raw * 1024 // Linux (and most others) report it in KB
        }
    }
}

/// Run `listen` against `src` for `duration`, tracking panics and peak-RSS
/// growth. See module doc for the overrun-tracking deviation. Returns an
/// error (not a panic report) if `listen()` itself returns `Err` -- e.g. no
/// signal found during startup calibration.
pub fn soak(src: AudioIqSource, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = stop.clone();
    let start = Instant::now();
    let baseline_rss = peak_rss_bytes();
    let mut worst_growth = 0u64;
    let mut event_count = 0usize;

    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration {
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(SAMPLE_INTERVAL.min(remaining.max(Duration::from_millis(1))));
        }
        stop_watchdog.store(true, Ordering::Relaxed);
    });

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        listen(src, cfg, stop.clone(), |_ev| {
            event_count += 1;
            if start.elapsed() >= WARMUP {
                let rss = peak_rss_bytes();
                worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
            }
        })
    }));
    let _ = watchdog.join();

    let panicked = match result {
        Ok(Ok(())) => false,
        Ok(Err(e)) => anyhow::bail!("listen() returned an error (not a panic): {e}"),
        Err(_) => true,
    };

    Ok(SoakReport {
        events_emitted: event_count,
        rss_growth_bytes: worst_growth,
        panicked,
    })
}

/// Pass/fail per ROADMAP's M1 gate (panic, unbounded memory).
pub fn soak_passed(report: &SoakReport) -> bool {
    !report.panicked && report.rss_growth_bytes < RSS_GROWTH_LIMIT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soak_reports_no_panic_on_a_clean_short_signal() {
        let fs = manta_input::TARGET_RATE_HZ;
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        let (env, _) = manta_testkit::keyer::key_text_loop(
            "CQ CQ DE W1AW W1AW K",
            &spec,
            fs as f64,
            8.0,
        )
        .unwrap();
        let mut real = vec![0.0f32; env.len()];
        let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
        let mut phi = 0.0f64;
        for (i, r) in real.iter_mut().enumerate() {
            *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
            phi += dphi;
        }
        let src = AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(
            real, fs,
        )))
        .unwrap();
        let report = soak(src, &PipelineConfig::default(), Duration::from_secs(1)).unwrap();
        assert!(!report.panicked);
        assert!(soak_passed(&report));
    }
}
```

Add `manta-testkit` to `crates/manta-engine/Cargo.toml`'s `[dev-dependencies]` (check first — it may already be there for other tests; `coppa-audio` was already added there in Task 5 Step 1).

- [ ] **Step 2: Register the module, run tests**

In `crates/manta-engine/src/lib.rs`, add:

```rust
pub mod soak;
pub use soak::{soak, soak_passed, SoakReport};
```

Run: `cargo test -p manta-engine soak:: -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Add the CLI `soak` subcommand**

In `crates/manta-cli/src/main.rs`'s `Command` enum:

```rust
    /// Run the listen pipeline for a fixed duration, checking for panics
    /// and unbounded memory growth (ROADMAP M1 accept criterion).
    Soak {
        /// Duration in seconds.
        #[arg(long)]
        duration: u64,
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
    },
```

In `main()`'s match:

```rust
        Command::Soak {
            duration,
            device,
            source,
        } => {
            let src = match source {
                Some(path) => manta_input::AudioIqSource::from_wav_file(&path)?,
                None => manta_input::AudioIqSource::from_device(device.as_deref())?,
            };
            let report = manta_engine::soak(
                src,
                &PipelineConfig::default(),
                std::time::Duration::from_secs(duration),
            )?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
```

- [ ] **Step 4: Add a CI-scoped soak test**

Create `crates/manta-cli/tests/soak_ci.rs` — this is the automated proxy for ROADMAP's "≥1 hour" gate, scaled to fit CI time: not a literal hour, but long enough (and against a long enough synthetic scene) to exercise the same code path for a sustained run without the actual wall-clock cost. Add `coppa-audio = { workspace = true }` to `crates/manta-cli/Cargo.toml`'s `[dev-dependencies]` if not already present (`manta-testkit` should already be there).

```rust
//! CI-scoped soak test: a genuinely automated proxy for ROADMAP's "runs >= 1
//! hour without panic or unbounded memory" M1 gate, run against a long
//! synthetic scene at file-replay pace rather than a literal wall-clock
//! hour. A real-hardware, real-duration soak is the manual runbook's job
//! (design doc §8), not CI's.
use manta_engine::{soak, soak_passed, PipelineConfig};
use manta_input::AudioIqSource;
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use std::time::Duration;

#[test]
fn soak_survives_a_sustained_run_without_panic_or_unbounded_memory() {
    let fs = manta_input::TARGET_RATE_HZ;
    let spec = KeyerSpec::new(25.0);
    let (env, _) = key_text_loop(
        "CQ CQ DE W1AW W1AW K CQ CQ DE VK9DX VK9DX K",
        &spec,
        fs as f64,
        120.0,
    )
    .unwrap();
    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }
    let src =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap();
    let report = soak(src, &PipelineConfig::default(), Duration::from_secs(120)).unwrap();
    assert!(!report.panicked, "soak panicked: {report:?}");
    assert!(soak_passed(&report), "soak failed: {report:?}");
}
```

Run: `cargo test -p manta-cli --test soak_ci -- --nocapture`
Expected: PASS, taking roughly 120 seconds (this is a real-time-scale soak of the streaming loop, not a wall-clock hour — proportionate for CI while still exercising sustained operation, per this task's framing).

- [ ] **Step 5: Commit**

```bash
git add crates/manta-engine/src/soak.rs crates/manta-engine/src/lib.rs crates/manta-engine/Cargo.toml crates/manta-cli/src/main.rs crates/manta-cli/tests/soak_ci.rs crates/manta-cli/Cargo.toml
git commit -m "feat(engine,cli): soak harness (panic + RSS growth) and manta soak subcommand"
```

---

### Task 11: Docs close-out

**Files:**
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Modify: `ARCHITECTURE.md`
- Create: `docs/DECISIONS/2026-07-17-m1-implementation-pins.md`
- Create: `docs/RUNBOOKS/m1-w1aw-live-copy.md`

- [ ] **Step 1: Update CLAUDE.md's Status line**

Change:
```
M0 implemented (single-signal WAV decode, V1 green); next is M1 in ROADMAP.md.
```
to:
```
M1 implemented (live audio decode, V1-V6 green); next is M2 in ROADMAP.md.
```
(Only after Steps 2-8's manual verification, below, confirms this is true — do not flip this line until the manual W1AW runbook has actually been run at least once, per this project's "claim of success comes with evidence" convention.)

- [ ] **Step 2: Update ARCHITECTURE.md's dependency graph**

In §2's dependency graph code block, add the new `manta-input → manta-dsp` edge:

```
manta-cli ──▶ manta-engine ──▶ manta-input ──▶ manta-dsp
                     │        ├──▶ manta-dsp ──────▶ coppa-dsp
                     │        ├──▶ manta-decode
                     │        └──▶ manta-spot
                     └──▶ manta-server
manta-testkit ──▶ manta-dsp, manta-decode, coppa-channel
```

Add a one-line note directly below the diagram: "M1 added `manta-input → manta-dsp` (the shared Hilbert transformer, used by both `AudioIqSource` and `manta-testkit`'s Watterson vector rendering) and `manta-testkit → coppa-channel` (Watterson fading, see the M1 pinned-decisions doc)."

- [ ] **Step 3: Update ARCHITECTURE.md's "Reused from coppa vs. new" table**

Add two rows:

```
| Audio-device capture, resampling, file replay | **reuse** `coppa-audio` |
| Real-to-analytic Hilbert conversion | **new** (`manta-dsp::hilbert`) — used by both live audio input and offline Watterson vector rendering |
```

- [ ] **Step 4: Write the M1 pinned-decisions doc**

Create `docs/DECISIONS/2026-07-17-m1-implementation-pins.md`, following the exact structure of `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`: a short intro pointing at this plan file, then a numbered list copied verbatim from this plan's "Deviations from the design doc" section plus pinned decision 20's resolution (cross-reference, don't duplicate the fix's rationale — link to `timing.rs`'s own doc comment), plus the coppa dependency bump:

```markdown
# M1 implementation pins

This is the M1 (`docs/superpowers/plans/2026-07-17-m1-live-audio-decode.md`)
implementation's pinned-decision record. Treat every numbered item below as
decided; SPEC and docs/ still win on anything not listed here.

## Deviations and pinned decisions

1. **Soak harness does not track input-overrun.** `coppa_audio::CpalSource`
   doesn't expose its internal ring's `overflow_count()` publicly, and
   file-replay sources (what the CI soak test runs against) have no ring and
   cannot overrun by construction. Live-hardware overrun observability needs
   a `coppa-audio` API addition -- out of scope for M1, tracked as a
   follow-up. `manta-engine::soak` checks panics and RSS growth only.
2. **The coppa commit pin lives in `Cargo.toml`/`Cargo.lock` + this doc, not
   per-vector `.manifest.json`** -- following M0's actual established
   convention (see the M0 pins doc's "coppa dependency pin" section), not
   the M1 design doc's phrasing.
3. **Pinned decision 20 (all-dah opener) is fixed, not just documented.**
   `crates/manta-decode/src/timing.rs`'s `ClusterPair::initialize()` now
   uses an absolute-ms prior (SPEC §4.1's `[20, 150]` ms dit clamp) instead
   of unconditionally assuming a lone unimodal cluster is dits. See that
   function's doc comment for the full fix rationale and the still-ambiguous
   60-150 ms middle band it does not (and cannot, from duration alone)
   resolve.
4. **CI's soak test is not a literal wall-clock hour.** It runs
   `manta-engine::soak` against a 120 s synthetic scene for a 120 s
   duration, proving the streaming loop survives sustained operation without
   panic/unbounded memory. ROADMAP's literal "≥1 hour" real-hardware gate is
   satisfied by the manual runbook (`docs/RUNBOOKS/m1-w1aw-live-copy.md`),
   not CI.

### coppa dependency pin bump

`coppa-dsp`/`coppa-audio`/`coppa-channel` are pinned in the workspace
`Cargo.toml` to git rev `f8a4d16df7e5776a0756943c05712038774e6c70` of
`https://github.com/HagaleTechnologies/coppa.git` (resolved from
`origin/main` HEAD on 2026-07-15; a descendant of both the M0 pin and the
2026-07-07 Watterson bug-fix commits `9ab1547`/`34aec5f`/`fc35895`).
```

- [ ] **Step 5: Write the manual W1AW runbook**

Create `docs/RUNBOOKS/m1-w1aw-live-copy.md`:

```markdown
# M1 manual acceptance: live W1AW copy

Not part of CI (design doc §8). Run this yourself against real rig audio
before flipping CLAUDE.md's Status line to "M1 implemented".

## W1AW code practice schedule

W1AW's CW code practice runs on a published weekday/weekend schedule across
multiple HF bands (check <http://www.arrl.org/code-and-emergency-comms> or
ARRL's current bulletin for the live schedule -- it's shifted over the
years, don't assume a fixed time). Pick a session at a comfortable
copying speed for a first pass.

## Steps

1. Connect your rig's RX audio output to your computer's audio input
   (sound card line-in, USB audio interface, or a rig with built-in USB
   audio CODEC).
2. Tune to a W1AW code-practice frequency/time slot, confirm you can hear
   clean CW in your normal audio monitoring path first.
3. `cargo run --release -p manta-cli -- listen --device <your interface name>`
   (omit `--device` to use the system default input; run
   `cargo run -p manta-cli -- listen --device nonexistent` first if
   unsure what device names are visible -- the error message won't list
   them today, so cross-check via your OS's audio settings panel).
4. Watch stdout. Expected: readable text tracking the code practice
   transmission (callsigns, "CQ", punctuation-adjacent prosigns) --- not
   necessarily perfect, but clearly *recognizable*, matching ROADMAP's M1
   bar.
5. Let it run at least several minutes to also eyeball basic stability
   (no panic, no runaway CPU/memory in Activity Monitor / htop).
6. Record the result (date, band, rough accuracy impression, any issues)
   in this file's "Runs" section below, or in a follow-up commit's message.

## Runs

(append entries here as you run this)
```

- [ ] **Step 6: Run this manual step yourself, or explicitly flag it as outstanding**

This step cannot be completed by an automated implementer. If you are an
agentic worker executing this plan, stop here and report back: "Task 11
Steps 1-5 (docs) are complete; Step 6 (the actual W1AW live-copy run) needs
Tony to run it against real hardware before Step 1's Status-line flip is
accurate." Do not mark CLAUDE.md's Status line as "M1 implemented" on your
own authority without this having actually happened.

- [ ] **Step 7: Full workspace verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace 2>&1 | tail -60`
Expected: clean on all three. Fix any clippy/fmt issues introduced across Tasks 1-10 before this commit.

- [ ] **Step 8: Commit**

```bash
git add ROADMAP.md CLAUDE.md ARCHITECTURE.md docs/DECISIONS/2026-07-17-m1-implementation-pins.md docs/RUNBOOKS/m1-w1aw-live-copy.md
git commit -m "docs: M1 close-out - pinned decisions, dependency-graph update, W1AW runbook"
```

- [ ] **Step 9: Push and open the PR**

```bash
git push -u origin feat/m1-live-audio
gh pr create --draft --title "M1: live audio, one signal" --body "Implements docs/superpowers/plans/2026-07-17-m1-live-audio-decode.md (design: docs/superpowers/specs/2026-07-17-m1-live-audio-design.md). V1-V6 green; all-dah opener bug (pinned decision 20) fixed; manual W1AW runbook still needs a real-hardware run before Status flips to M1-complete."
```

---

## Post-plan check

Before declaring M1 done: all 11 tasks' tests green, `cargo test --workspace` clean, the manual W1AW runbook actually run at least once with a recognizable result, and CLAUDE.md's Status line only then flipped.
