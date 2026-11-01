# M2 sub-project 1 — PFB Channelizer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A real N-channel WOLA polyphase filterbank (SPEC-decode-core.md §1) replaces the M0/M1 single-channel shim (`SingleChannelExtractor`/`estimate_peak_hz`) in `manta-engine`'s batch and streaming paths, with V1–V6 passing unchanged and new multi-signal tests proving the channelizer actually separates simultaneous signals for the first time in this codebase.

**Architecture:** New `manta-dsp::channelizer` module implements the WOLA fold + circular phase-correction + FFT + power pipeline over all N channels every hop, reusing `proto::design_prototype` (already channel-count-generic) and `coppa_dsp::fft::FftProcessor` unchanged. A minimal placeholder detector (one-time calibration-window argmax over real per-channel power) picks a single channel `k0`; `TrackDecoder` is untouched — it just receives channel `k0`'s magnitude stream instead of the old extractor's output. `manta-dsp::single`/`freqest` are deprecated in place (doc comments only), not deleted.

**Tech Stack:** Rust (edition 2021, rust-version 1.85.0), reuses `coppa-dsp::fft::FftProcessor` and `manta-dsp::proto` unchanged, no new dependencies.

**Design doc:** `docs/superpowers/specs/2026-07-18-m2-pfb-channelizer-design.md` — read it first; this plan implements it section by section.

## Global Constraints

- **Determinism (SPEC §6):** NO RNG, NO wall clock in the channelizer or placeholder detector. Long accumulations run **sequentially in `f64`** — this applies to the WOLA fold sum (8 terms per bin, accumulated via `f64` intermediates before the FFT) exactly as it already applies to `single.rs`'s direct FIR sum and `proto.rs`'s prototype design.
- **Dimensions (SPEC §1.1):** `N = fs / 93.75` must be a power of two for all supported table rates (1024@96k, 2048@192k, 4096@384k, 8192@768k). `hop = N/4`. Output rate `fo = 375 Hz` is invariant across input rates — nothing downstream (including `manta-decode`) depends on `fs`.
- **Prototype filter (SPEC §1.2):** reuse `manta_dsp::proto::design_prototype(n_channels, taps_per_branch)` unchanged — it was already written generically at M0. Do not duplicate or modify the Kaiser/windowed-sinc design.
- **Power-to-dB epsilon (SPEC §1.3/§1.4):** `PdB = 10·log10(P + ε)`, **ε = 1e-20** exactly (not `freqest.rs`'s `1e-30` — that was the M0 shim's own choice; the new channelizer follows SPEC's stated value).
- **coppa reuse boundary:** `coppa_dsp::fft::FftProcessor` is reused exactly as-is (`new(size)`, `forward(&[Complex32]) -> Vec<Complex32>`, unnormalized, panics if `input.len() != size`). No new FFT code.
- **CI:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` all clean — run the **full workspace** command for clippy, not a per-crate scope (M1's plan hit this exact mistake twice; a per-crate clean run does not guarantee the real CI gate passes).
- **Multi-agent hygiene (CLAUDE.md):** work on a branch (`feat/m2-pfb-channelizer`), push early, open a draft PR as the claim, `--force-with-lease` only, main moves only by PR merge.
- Rustdoc comments on every public item cite the SPEC section they implement.

---

### Task 1: Channelizer core (`manta-dsp::channelizer`)

**Files:**
- Create: `crates/manta-dsp/src/channelizer.rs`
- Modify: `crates/manta-dsp/src/lib.rs`

**Interfaces:**
- Consumes: `crate::proto::{design_prototype, TAPS_PER_BRANCH}` (existing, unchanged), `coppa_dsp::fft::FftProcessor` (existing, unchanged).
- Produces: `pub struct HopOutput { pub m: u64, pub x: Vec<Complex32>, pub power: Vec<f32> }`, `pub struct Channelizer`, `impl Channelizer { pub fn new(fs: f64, center_freq_hz: f64) -> Result<Self, String>; pub fn n_channels(&self) -> usize; pub fn hop(&self) -> usize; pub fn filter_len(&self) -> usize; pub fn channel_freq_hz(&self, k: usize) -> f64; pub fn process(&mut self, iq: &[Complex32]) -> Vec<HopOutput> }`, `pub fn power_db(power: f32) -> f64`. Used by Task 2 (interpolator), Task 3 (multi-signal tests), Task 5/6/7 (engine wiring).

- [ ] **Step 1: Write the failing tests**

Create `crates/manta-dsp/src/channelizer.rs`:

```rust
//! WOLA polyphase filterbank channelizer (SPEC §1.1-1.3): the full
//! N-channel successor to the M0 single-channel shim (`single.rs`).

use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use coppa_dsp::fft::FftProcessor;
use num_complex::{Complex32, Complex64};

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1
/// SPEC §1.3/§1.4 power-to-dB epsilon.
const POWER_DB_EPSILON: f64 = 1e-20;

/// One hop's channelizer output: per-channel complex spectrum and power.
/// SPEC §1.3.
#[derive(Debug, Clone)]
pub struct HopOutput {
    /// Hop index, monotonically increasing from stream start.
    pub m: u64,
    /// Complex per-channel spectrum, FFT bin order (index k; SPEC §1.1's
    /// f(k) mapping applies via `Channelizer::channel_freq_hz`).
    pub x: Vec<Complex32>,
    /// Per-channel power, `|X[k]|^2`. SPEC §1.3 step 4.
    pub power: Vec<f32>,
}

/// `PdB = 10*log10(P + epsilon)`, SPEC §1.3/§1.4's epsilon = 1e-20.
pub fn power_db(power: f32) -> f64 {
    10.0 * (power as f64 + POWER_DB_EPSILON).log10()
}

/// WOLA polyphase filterbank: N channels, 4x oversampled (hop = N/4).
/// SPEC §1.1-1.3.
pub struct Channelizer {
    n: usize,
    hop: usize,
    taps: Vec<f32>, // length L*N, L = TAPS_PER_BRANCH
    fft: FftProcessor,
    /// Sliding input window; `read` indexes the next window start. Samples
    /// before `read` are dead and get compacted away (same pattern as
    /// `single.rs::SingleChannelExtractor`).
    buf: Vec<Complex32>,
    read: usize,
    /// Hop output counter, used for the SPEC §1.3 step-2 rotation `r =
    /// (m*hop) mod N`. A plain integer counter, not an accumulated phase --
    /// no drift/precision concern (unlike an NCO), so no need to derive it
    /// from an absolute sample index.
    m: u64,
    fs: f64,
    center_freq_hz: f64,
}

impl Channelizer {
    /// A channelizer for a supported table rate (`fs/93.75` a power of
    /// two). SPEC §1.1.
    pub fn new(fs: f64, center_freq_hz: f64) -> Result<Self, String> {
        let nf = fs / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!(
                "unsupported sample rate {fs}: fs/93.75 must be a power of two"
            ));
        }
        Ok(Channelizer {
            n,
            hop: n / 4,
            taps: design_prototype(n, TAPS_PER_BRANCH),
            fft: FftProcessor::new(n),
            buf: Vec::new(),
            read: 0,
            m: 0,
            fs,
            center_freq_hz,
        })
    }

    /// Number of channels, N. SPEC §1.1.
    pub fn n_channels(&self) -> usize {
        self.n
    }

    /// Input samples consumed per output hop (N/4). SPEC §1.1.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Prototype filter length in taps (L*N). Same causal-filter blind-zone
    /// property as `single.rs`'s extractor -- see the M0 lead-in-padding
    /// fix in `manta-engine`, which Task 6/7 apply here too.
    pub fn filter_len(&self) -> usize {
        self.taps.len()
    }

    /// Channel `k`'s RF center frequency. SPEC §1.1:
    /// `f(k) = f_center + ((k + N/2) mod N - N/2) * Delta`.
    pub fn channel_freq_hz(&self, k: usize) -> f64 {
        let delta = self.fs / self.n as f64;
        let signed = ((k + self.n / 2) % self.n) as f64 - (self.n / 2) as f64;
        self.center_freq_hz + signed * delta
    }

    /// Feed input IQ; returns however many hops became available. SPEC §1.3.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<HopOutput> {
        self.buf.extend_from_slice(iq);
        let ln = self.taps.len();
        let mut outputs = Vec::new();
        while self.read + ln <= self.buf.len() {
            let window = &self.buf[self.read..self.read + ln];

            // Step 1: window & fold. u[n] = x[n]*h[LN-1-n]; v[j] = sum_p
            // u[j + p*N]. Sequential f64 accumulation (SPEC §6.4 convention,
            // matching single.rs's direct-FIR sum).
            let mut v = vec![Complex64::new(0.0, 0.0); self.n];
            for (n_idx, &x) in window.iter().enumerate() {
                let h = self.taps[ln - 1 - n_idx] as f64;
                let j = n_idx % self.n;
                v[j].re += h * x.re as f64;
                v[j].im += h * x.im as f64;
            }
            let v: Vec<Complex32> = v.iter().map(|c| Complex32::new(c.re as f32, c.im as f32)).collect();

            // Step 2: circular rotation left by r = (m*hop) mod N.
            let r = ((self.m.wrapping_mul(self.hop as u64)) % self.n as u64) as usize;
            let mut v_rot = vec![Complex32::new(0.0, 0.0); self.n];
            for j in 0..self.n {
                v_rot[j] = v[(j + r) % self.n];
            }

            // Step 3: FFT. Step 4: power.
            let x = self.fft.forward(&v_rot);
            let power: Vec<f32> = x.iter().map(|c| c.norm_sqr()).collect();
            outputs.push(HopOutput { m: self.m, x, power });

            self.m += 1;
            self.read += self.hop;
        }
        if self.read >= ln {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        outputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    /// Channel index for offset_hz at the given N/fs (SPEC §1.1's f(k)
    /// inverted): `k = ((round(offset/delta)) mod N + N) mod N`.
    fn channel_for_offset(offset_hz: f64, n: usize, fs: f64) -> usize {
        let delta = fs / n as f64;
        let k_signed = (offset_hz / delta).round() as i64;
        k_signed.rem_euclid(n as i64) as usize
    }

    #[test]
    fn rejects_non_table_rate() {
        assert!(Channelizer::new(44_100.0, 0.0).is_err());
        assert!(Channelizer::new(96_000.0, 0.0).is_ok());
        assert!(Channelizer::new(192_000.0, 0.0).is_ok());
    }

    #[test]
    fn dimensions_match_spec_table() {
        let ch = Channelizer::new(96_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 1024);
        assert_eq!(ch.hop(), 256);
        let ch = Channelizer::new(192_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 2048);
        assert_eq!(ch.hop(), 512);
    }

    #[test]
    fn channel_freq_hz_matches_spec_f_of_k() {
        let ch = Channelizer::new(96_000.0, 14_000_000.0).unwrap();
        // k=0 is DC (center frequency itself).
        assert!((ch.channel_freq_hz(0) - 14_000_000.0).abs() < 1e-6);
        // k=1 is one channel above center.
        assert!((ch.channel_freq_hz(1) - (14_000_000.0 + 93.75)).abs() < 1e-6);
        // k = N-1 wraps to one channel BELOW center (negative-frequency side).
        assert!((ch.channel_freq_hz(1023) - (14_000_000.0 - 93.75)).abs() < 1e-6);
    }

    #[test]
    fn on_channel_tone_settles_near_unity_in_its_own_channel() {
        // Tone exactly at channel k0=64's center: offset = 64*93.75 = 6000 Hz.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        assert_eq!(k0, 64);
        let iq = tone(6_000.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!((mag - 1.0).abs() < 0.05, "channel {k0} magnitude {mag}");
        }
    }

    #[test]
    fn tone_150hz_away_is_rejected_by_80db_in_the_home_channel() {
        // SPEC §1.2: alias rejection >= 80 dB from ~108 Hz (1.15 channels) away.
        // A tone at k0's center + 150 Hz lands in a DIFFERENT home channel;
        // k0 itself should show near-zero power from it.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let iq = tone(6_000.0 + 150.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!(mag < 2e-4, "stopband leak into channel {k0}: {mag}"); // -74 dB, slack for f32
        }
    }

    #[test]
    fn channel_edge_is_minus_6_db_in_both_neighbors() {
        // Tone exactly between channels k0 and k0+1 (edge = k0*Delta + Delta/2).
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let edge_hz = 6_000.0 + 93.75 / 2.0;
        let iq = tone(edge_hz, 384_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let steady = &hops[warmup..];
        let mean_mag = |k: usize| -> f32 {
            let sum: f32 = steady.iter().map(|h| h.power[k].sqrt()).sum();
            sum / steady.len() as f32
        };
        // -6 dB = 0.501 in amplitude; both neighbors should sit near 0.5.
        assert!((mean_mag(k0) - 0.5).abs() < 0.05, "k0 edge gain {}", mean_mag(k0));
        assert!(
            (mean_mag(k0 + 1) - 0.5).abs() < 0.05,
            "k0+1 edge gain {}",
            mean_mag(k0 + 1)
        );
    }

    #[test]
    fn is_deterministic() {
        let iq = tone(6_000.0, 96_000, 1.0, FS);
        let mut ch_a = Channelizer::new(FS, 0.0).unwrap();
        let mut ch_b = Channelizer::new(FS, 0.0).unwrap();
        let hops_a = ch_a.process(&iq);
        let hops_b = ch_b.process(&iq);
        assert_eq!(hops_a.len(), hops_b.len());
        for (a, b) in hops_a.iter().zip(hops_b.iter()) {
            for (pa, pb) in a.power.iter().zip(b.power.iter()) {
                assert_eq!(pa.to_bits(), pb.to_bits());
            }
        }
    }

    #[test]
    fn power_db_matches_spec_epsilon() {
        // 10*log10(0 + 1e-20) = -200.0 exactly.
        assert!((power_db(0.0) - (-200.0)).abs() < 1e-9);
        // 10*log10(1.0 + 1e-20) ~ 0.0.
        assert!(power_db(1.0).abs() < 1e-6);
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let iq = tone(6_000.0, 20_000, 1.0, FS);
        let mut whole = Channelizer::new(FS, 0.0).unwrap();
        let hops_whole = whole.process(&iq);

        let mut chunked = Channelizer::new(FS, 0.0).unwrap();
        let mut hops_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            hops_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(hops_whole.len(), hops_chunked.len());
        for (a, b) in hops_whole.iter().zip(hops_chunked.iter()) {
            assert_eq!(a.x, b.x);
        }
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-dsp/src/lib.rs`, add `channelizer` to the module list (alongside the existing `freqest`, `hilbert`, `proto`, `single`):

```rust
pub mod channelizer;
```

Update the crate-level doc comment at the top of `lib.rs` to mention the channelizer supersedes `single`/`freqest` (Task 4 will add the formal deprecation notices to those two files themselves; this is just the crate doc's one-line summary).

- [ ] **Step 3: Run tests to verify they fail, then build**

Run: `cargo test -p manta-dsp channelizer:: 2>&1 | tail -30`
Expected: FAIL to compile until Step 1's file exists (it does, since Step 1 wrote the full implementation inline with its tests — same pattern M1's Task 3 used for the Hilbert transformer). Build and get to Step 4.

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p manta-dsp channelizer:: -- --nocapture`
Expected: PASS on all 9 tests. If `channel_freq_hz_matches_spec_f_of_k` or `channel_for_offset` disagree on sign conventions, re-check against `freqest.rs`'s existing, already-correct `estimate_peak_hz` (lines 71-78) — it implements the same SPEC §1.1 formula and its sign convention (`>=` not `>` at the Nyquist wrap) is the proven reference.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-dsp/src/channelizer.rs crates/manta-dsp/src/lib.rs
git commit -m "feat(dsp): WOLA polyphase filterbank channelizer (SPEC §1.1-1.3)"
```

---

### Task 2: Fine-frequency interpolator (SPEC §1.4)

**Files:**
- Modify: `crates/manta-dsp/src/channelizer.rs`

**Interfaces:**
- Consumes: `power_db` (Task 1, same file).
- Produces: `pub fn interpolate_offset(p_minus: f32, p_zero: f32, p_plus: f32) -> Option<f64>`.

- [ ] **Step 1: Write the failing tests**

Add to `channelizer.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn interpolate_offset_finds_symmetric_peak_at_zero() {
        // A true local max with equal neighbors -> delta = 0.
        assert_eq!(interpolate_offset(0.5, 1.0, 0.5), Some(0.0));
    }

    #[test]
    fn interpolate_offset_leans_toward_the_larger_neighbor() {
        // Peak biased toward p_plus -> positive delta (SPEC §1.4 formula
        // sign convention: delta = 0.5*(P_minus - P_plus)/denom).
        let d = interpolate_offset(0.3, 1.0, 0.6).unwrap();
        assert!(d > 0.0, "delta {d}");
        assert!(d <= 0.5);
    }

    #[test]
    fn interpolate_offset_clamps_to_half_bin() {
        // Extremely asymmetric neighbors would produce |delta| > 0.5 unclamped.
        let d = interpolate_offset(1e-6, 1.0, 0.999).unwrap();
        assert!((-0.5..=0.5).contains(&d));
    }

    #[test]
    fn interpolate_offset_none_when_not_a_local_max() {
        // Monotonically increasing power (no peak at the center bin):
        // denom = P_minus - 2*P_zero + P_plus >= 0 -> unusable.
        assert_eq!(interpolate_offset(0.1, 0.2, 0.3), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p manta-dsp channelizer::tests::interpolate 2>&1 | tail -20`
Expected: FAIL to compile (`interpolate_offset` doesn't exist yet).

- [ ] **Step 3: Implement `interpolate_offset`**

Add to `channelizer.rs` (near `power_db`):

```rust
/// Per-hop fine-frequency interpolation (SPEC §1.4): quadratic
/// interpolation on dB powers of the three bins around a candidate peak
/// channel. Returns the sub-bin offset in `[-0.5, 0.5]`, or `None` if the
/// hop is "unusable" (no local maximum at the center bin).
pub fn interpolate_offset(p_minus: f32, p_zero: f32, p_plus: f32) -> Option<f64> {
    let pm = power_db(p_minus);
    let p0 = power_db(p_zero);
    let pp = power_db(p_plus);
    let denom = pm - 2.0 * p0 + pp;
    if denom < 0.0 {
        Some((0.5 * (pm - pp) / denom).clamp(-0.5, 0.5))
    } else {
        None
    }
}
```

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p manta-dsp channelizer:: -- --nocapture`
Expected: PASS on all 13 tests (9 from Task 1 + 4 new).

- [ ] **Step 5: Commit**

```bash
git add crates/manta-dsp/src/channelizer.rs
git commit -m "feat(dsp): fine-frequency quadratic interpolation (SPEC §1.4)"
```

---

### Task 3: Multi-signal wideband tests + second sample rate

**Files:**
- Create: `crates/manta-testkit/tests/channelizer_multisignal.rs`

**Interfaces:**
- Consumes: `manta_dsp::channelizer::Channelizer` (Task 1), `manta_testkit::scene::{render_scene, SignalSpec}` (existing).
- Produces: nothing new — this test proves the design doc's core claim (§4: "the first real proof the PFB actually separates simultaneous signals"), entirely via existing public APIs.

- [ ] **Step 1: Write the tests**

Create `crates/manta-testkit/tests/channelizer_multisignal.rs`:

```rust
//! Proves the M2 channelizer design's core claim (design doc §4): the WOLA
//! filterbank actually separates multiple simultaneous signals into their
//! correct channels -- never exercised by V1-V6, which are all
//! single-signal scenes.

use num_complex::Complex32;
use manta_dsp::channelizer::Channelizer;
use manta_testkit::scene::{render_scene, SignalSpec};

fn channel_for_offset(offset_hz: f64, n: usize, fs: f64) -> usize {
    let delta = fs / n as f64;
    let k_signed = (offset_hz / delta).round() as i64;
    k_signed.rem_euclid(n as i64) as usize
}

fn signal(offset_hz: f64) -> SignalSpec {
    SignalSpec {
        text: "VVV VVV VVV".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
    }
}

fn assert_channels_resolved(samples: &[Complex32], fs: f64, offsets: &[f64]) {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let hops = ch.process(samples);
    let warmup = ch.filter_len() / ch.hop() + 1;
    assert!(hops.len() > warmup, "not enough hops past warm-up");
    let steady = &hops[warmup..];

    let n = ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in steady {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    for p in &mut avg_power {
        *p /= steady.len() as f64;
    }

    let mut sorted = avg_power.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[n / 2];

    for &off in offsets {
        let k = channel_for_offset(off, n, fs);
        assert!(
            avg_power[k] > median * 10.0,
            "channel {k} (offset {off} Hz) power {} not clearly above noise-floor proxy {median}",
            avg_power[k]
        );
    }
}

#[test]
fn resolves_three_simultaneous_signals_at_96khz() {
    let fs = 96_000.0;
    let offsets = [5_000.0, 10_000.0, -7_500.0];
    let signals: Vec<SignalSpec> = offsets.iter().map(|&o| signal(o)).collect();
    let (samples, _) = render_scene(&signals, fs, 5.0, Some(42)).unwrap();
    assert_channels_resolved(&samples, fs, &offsets);
}

#[test]
fn resolves_three_simultaneous_signals_at_192khz() {
    // Same scene, doubled sample rate (N=2048 instead of 1024) -- proves
    // the N = fs/93.75 generalization holds for a second table rate, not
    // just the one every existing fixture (V1-V6) happens to use.
    let fs = 192_000.0;
    let offsets = [15_000.0, -30_000.0, 40_000.0];
    let signals: Vec<SignalSpec> = offsets.iter().map(|&o| signal(o)).collect();
    let (samples, _) = render_scene(&signals, fs, 5.0, Some(43)).unwrap();
    assert_channels_resolved(&samples, fs, &offsets);
}

#[test]
fn resolves_signals_with_different_snrs() {
    // A strong and a weak signal together -- proves the weaker one isn't
    // swamped by leakage from the stronger one's stopband.
    let fs = 96_000.0;
    let mut strong = signal(8_000.0);
    strong.snr_2500_db = 25.0;
    let mut weak = signal(-12_000.0);
    weak.snr_2500_db = 8.0;
    let (samples, _) = render_scene(&[strong, weak], fs, 5.0, Some(44)).unwrap();
    assert_channels_resolved(&samples, fs, &[8_000.0, -12_000.0]);
}
```

- [ ] **Step 2: Run tests, verify all pass**

Run: `cargo test -p manta-testkit --test channelizer_multisignal -- --nocapture`
Expected: PASS on all 3 tests. If a channel fails to clearly separate, check: (a) the offsets are far enough apart (several channel-spacings, not adjacent bins — adjacent-bin behavior is already covered by Task 1's edge test, not this one's job); (b) `assert_channels_resolved`'s `median * 10.0` margin isn't too tight for the chosen SNRs — prefer widening the offset separation or SNR over loosening this margin, since the point is proving clean separation, not a borderline pass.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -40`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/manta-testkit/tests/channelizer_multisignal.rs
git commit -m "test(testkit): prove channelizer separates simultaneous signals (2 sample rates)"
```

---

### Task 4: Deprecate `manta-dsp::single`/`freqest`

**Files:**
- Modify: `crates/manta-dsp/src/single.rs`
- Modify: `crates/manta-dsp/src/freqest.rs`

**Interfaces:** none changed — doc comments only, no behavior change, no test change.

- [ ] **Step 1: Update `single.rs`'s module doc comment**

Replace the file's top doc comment:

```rust
//! Single-channel extractor: one PFB channel computed directly (M0 shim).
//! Mix by -offset, prototype lowpass, decimate by N/4 to 375 Hz.
//! Superseded by the full PFB (SPEC §1.3) at M2.
```

with:

```rust
//! **Deprecated** as of M2 sub-project 1 (`manta-dsp::channelizer`
//! implements the real WOLA polyphase filterbank, SPEC §1.3). Kept
//! compiled and tested for now as a reference/fallback -- not wired into
//! `manta-engine` anymore as of that sub-project. Candidate for removal
//! once the channelizer path has run cleanly for a few months; see
//! `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`.
//!
//! Single-channel extractor: one PFB channel computed directly (M0 shim).
//! Mix by -offset, prototype lowpass, decimate by N/4 to 375 Hz.
```

- [ ] **Step 2: Update `freqest.rs`'s module doc comment**

Replace:

```rust
//! M0 frequency finder: averaged periodogram + parabolic interpolation.
//! Uses the same dB-domain delta formula and clamp as SPEC §1.4. Superseded
//! by the PFB detector + track centroid at M2.
```

with:

```rust
//! **Deprecated** as of M2 sub-project 1 -- `manta-dsp::channelizer`'s
//! real per-channel power output plus its `interpolate_offset` (SPEC §1.4)
//! replace this periodogram-based estimator. Kept compiled and tested for
//! now as a reference/fallback; not wired into `manta-engine` anymore as
//! of that sub-project. Candidate for removal once the channelizer path has
//! run cleanly for a few months; see
//! `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`.
//!
//! M0 frequency finder: averaged periodogram + parabolic interpolation.
//! Uses the same dB-domain delta formula and clamp as SPEC §1.4.
```

- [ ] **Step 3: Run tests, verify unchanged**

Run: `cargo test -p manta-dsp single:: freqest:: -- --nocapture`
Expected: PASS, identical to before (doc-comment-only change).

- [ ] **Step 4: Commit**

```bash
git add crates/manta-dsp/src/single.rs crates/manta-dsp/src/freqest.rs
git commit -m "docs(dsp): mark single.rs/freqest.rs deprecated, superseded by channelizer.rs"
```

---

### Task 5: Placeholder detector (`manta-engine`)

**Files:**
- Create: `crates/manta-engine/src/detect.rs`
- Modify: `crates/manta-engine/src/lib.rs`

**Interfaces:**
- Consumes: `manta_dsp::channelizer::Channelizer` (Task 1).
- Produces: `pub(crate) fn calibrate_channel(ch: &mut Channelizer, calib_iq: &[num_complex::Complex32]) -> Option<usize>` — deliberately crate-private (design doc §2: "not SPEC §2's real detector... a later sub-project's job"), so it doesn't read as permanent public API.

- [ ] **Step 1: Write the failing test**

Create `crates/manta-engine/src/detect.rs`:

```rust
//! Placeholder detector (design doc §2): a deliberately minimal stand-in
//! for SPEC §2's real order-statistic detector + track manager (a later M2
//! sub-project). Picks the loudest channel once, over a fixed calibration
//! window, and never re-evaluates -- exactly enough to keep M0/M1's
//! single-track decode path working through the new channelizer.

use num_complex::Complex32;
use manta_dsp::channelizer::Channelizer;

/// Run `ch` over `calib_iq`, return the channel index with the highest
/// average power across the resulting hops, or `None` if no hops were
/// produced (calibration window shorter than one filter length).
pub(crate) fn calibrate_channel(ch: &mut Channelizer, calib_iq: &[Complex32]) -> Option<usize> {
    let hops = ch.process(calib_iq);
    if hops.is_empty() {
        return None;
    }
    let n = ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in &hops {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    let mut k0 = 0;
    for (k, &p) in avg_power.iter().enumerate() {
        if p > avg_power[k0] {
            k0 = k;
        }
    }
    Some(k0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn finds_the_loudest_channel() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 96_000, 1.0, FS); // 1 s calibration window
        let k0 = calibrate_channel(&mut ch, &iq).unwrap();
        assert_eq!(k0, 64); // 6000 / 93.75 = 64 exactly
    }

    #[test]
    fn none_on_too_short_input() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 10, 1.0, FS); // far shorter than one filter length
        assert!(calibrate_channel(&mut ch, &iq).is_none());
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-engine/src/lib.rs`, add near the top:

```rust
mod detect;
```

(crate-private module, not `pub mod` — matches `calibrate_channel`'s `pub(crate)` visibility.)

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p manta-engine detect:: -- --nocapture`
Expected: PASS on both tests.

- [ ] **Step 4: Commit**

```bash
git add crates/manta-engine/src/detect.rs crates/manta-engine/src/lib.rs
git commit -m "feat(engine): placeholder single-channel detector over the new channelizer"
```

---

### Task 6: Wire the batch path (`decode_samples`/`decode_wav`)

**Files:**
- Modify: `crates/manta-engine/src/lib.rs`

**Interfaces:**
- Consumes: `manta_dsp::channelizer::Channelizer` (Task 1), `crate::detect::calibrate_channel` (Task 5).
- Produces: `decode_samples`/`decode_wav`'s public signatures are unchanged; only their internals swap extractor.

- [ ] **Step 1: Read the current `decode_samples` in full**

Read `crates/manta-engine/src/lib.rs`'s current `decode_samples` (the M0 batch pipeline: `estimate_peak_hz` → `SingleChannelExtractor` → lead-in padding → `TrackDecoder`) before editing, to confirm this plan's replacement code lines up exactly with what's there now.

- [ ] **Step 2: Replace the extractor construction and processing**

Replace the body of `decode_samples` (keep the existing degenerate-input guard, the `TrackDecoder::new`/`set_freq_hz` calls, and the event-collection/report-assembly tail unchanged) with:

```rust
pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }

    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channelizer")?;
    let hop = ch.hop() as u64;

    debug_assert!(
        (fs / hop as f64 - manta_decode::FO_HZ).abs() < 0.01,
        "channelizer hop rate {} Hz diverges from manta_decode::FO_HZ {}",
        fs / hop as f64,
        manta_decode::FO_HZ
    );

    // Calibration pass (design doc §2): find the loudest channel over a
    // fixed-length window, matching the M0/M1 approach of a one-time
    // startup estimate. Reuse the whole input for calibration, same as M0's
    // decode_samples always did (batch mode has the whole file up front).
    let Some(k0) = crate::detect::calibrate_channel(&mut ch, iq) else {
        bail!("no signal found (input shorter than one filter length or empty)");
    };
    let offset_hz = ch.channel_freq_hz(k0) - center_freq_hz;

    // Same lead-in group-delay fix as the M0 shim (pinned decision 19):
    // the channelizer's causal FIR window has an identical blind-zone
    // property at stream start. Re-run calibration's channel choice, but
    // reprocess from a padded, freshly-constructed channelizer so every
    // hop (including the padding/real-signal boundary) is fed to the
    // decoder -- calibrate_channel already consumed `iq` once above just to
    // pick k0; that channelizer instance is discarded, not reused, so this
    // second pass starts from a clean internal buffer.
    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(center_freq_hz + offset_hz);

    let hops = ch.process(&padded_iq);
    let mut events: Vec<DecoderEvent> = Vec::new();
    for hop_out in &hops {
        let sample_ts = hop_out.m.saturating_sub(pad_hops) * hop;
        let mag = hop_out.power[k0].sqrt();
        events.extend(decoder.push_envelope(mag, sample_ts));
    }
    events.extend(decoder.finish());

    let wpm = events.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&events);
    Ok(DecodeReport {
        freq_hz: center_freq_hz + offset_hz,
        wpm,
        text,
        events,
    })
}
```

Remove the now-unused `use manta_dsp::freqest::estimate_peak_hz;` and `use manta_dsp::single::SingleChannelExtractor;` imports from this file's top (they were the M0 shim's imports; `manta_dsp::channelizer` and `crate::detect` replace them).

- [ ] **Step 3: Run the golden regression suite**

Run: `cargo test -p manta-cli 2>&1 | tail -100`
Expected: V1, V2, V3, V4, V6 pass; V5 shows `ignored` (unchanged from M1 — this task doesn't touch decode accuracy under fading, only the channel-selection mechanism). If any of V1-V4/V6 regress, this is real signal about the swap's correctness — likely candidates: `k0`'s power at index `hop_out.power[k0]` using the wrong channel after the second (padded) channelizer's own internal calibration state differs from the first pass's (it shouldn't, since both instances process the same underlying real signal, just one is padded) -- double check `ch.channel_freq_hz(k0) - center_freq_hz` computes the same `offset_hz` the old `estimate_peak_hz` would have for these fixtures (SPEC §1.1's channel granularity is 93.75 Hz, coarser than the old periodogram's continuous estimate -- V1-V6's fixture offsets were not necessarily chosen to land on exact channel centers, so `TrackDecoder`'s reported `freq_hz` may shift slightly; this affects `report.freq_hz` in golden tests, not `report.text`, so text-based CER assertions should be unaffected, but check `golden_v1.rs`'s explicit `freq error <= 10 Hz` assertion carefully -- SPEC's channel spacing (93.75 Hz) means a channel-quantized offset alone could exceed 10 Hz error; this is *expected* to need SPEC §1.4's fine-frequency interpolation to recover accuracy, which is not yet wired in this task (Task 1/2 built `interpolate_offset` as a pure function, but no caller in this task feeds it real hop data yet) -- if `golden_v1.rs`'s freq-error assertion fails for exactly this reason, that is a real, expected gap this task's escalation criteria should catch, not something to route around.

- [ ] **Step 4: If the freq-error assertion fails, wire in the fine-frequency interpolator**

If Step 3 shows `golden_v1.rs`'s `freq error <= 10 Hz` assertion failing (the expected consequence of channel-quantized offset alone, per Step 3's note), extend `decode_samples` to accumulate the SPEC §1.4 track centroid: for each hop where `k0`'s power is a local max (`interpolate_offset(hop_out.power[k0-1], hop_out.power[k0], hop_out.power[k0+1])` returns `Some(delta)`), accumulate `(k0 + delta) * power[k0]` and `power[k0]` into running `f64` sums; after the loop, `centroid = sum_weighted / sum_power` (default to `k0 as f64` if no hop ever had a local max), and use `center_freq_hz + (centroid - k0) * (fs / ch.n_channels() as f64) + offset_hz` in place of the plain channel-quantized `offset_hz` for the final `freq_hz` report (do NOT change what feeds `TrackDecoder` -- decode accuracy doesn't depend on this refinement, only the reported frequency's precision does). Re-run Step 3 to confirm.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -40`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-engine/src/lib.rs
git commit -m "feat(engine): wire the channelizer + placeholder detector into decode_samples/decode_wav"
```

---

### Task 7: Wire the streaming path (`listen`) + new chunking-determinism test

**Files:**
- Modify: `crates/manta-engine/src/listen.rs`
- Create: `crates/manta-engine/tests/channelizer_chunking_determinism.rs`

**Interfaces:**
- Consumes: `manta_dsp::channelizer::Channelizer` (Task 1), `crate::detect::calibrate_channel` (Task 5).
- Produces: `listen`'s public signature is unchanged.

- [ ] **Step 1: Read the current `listen` in full**

Read `crates/manta-engine/src/listen.rs`'s current implementation (calibration window → `estimate_peak_hz` → `SingleChannelExtractor` → lead-in padding → main read loop → `decoder.finish()`) before editing.

- [ ] **Step 2: Replace the extractor construction and per-chunk processing**

Replace the body of `listen` (keep the function signature, the calibration-window READ loop that fills `calib`, and the final `decoder.finish()` unchanged) with:

```rust
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

    let mut calib_ch = manta_dsp::channelizer::Channelizer::new(fs, 0.0)
        .map_err(|e| anyhow::anyhow!(e))?;
    let k0 = crate::detect::calibrate_channel(&mut calib_ch, &calib)
        .context("no signal found during startup calibration")?;
    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, 0.0).map_err(|e| anyhow::anyhow!(e))?;
    let offset_hz = ch.channel_freq_hz(k0);
    let hop = ch.hop() as u64;
    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(offset_hz);

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    let mut m: u64 = 0;
    for hop_out in ch.process(&padding) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
            on_event(&ev);
        }
        m += 1;
    }
    for hop_out in ch.process(&calib) {
        let sample_ts = m.saturating_sub(pad_hops) * hop;
        for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
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
            break;
        }
        for hop_out in ch.process(&chunk[..n]) {
            let sample_ts = m.saturating_sub(pad_hops) * hop;
            for ev in decoder.push_envelope(hop_out.power[k0].sqrt(), sample_ts) {
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

Remove the now-unused `use manta_dsp::freqest::estimate_peak_hz;` and `use manta_dsp::single::SingleChannelExtractor;` imports from this file's top.

Note: `calib_ch` (used only for calibration) and `ch` (used for the real padded run) are two separate `Channelizer` instances, exactly mirroring Task 6's batch-path pattern (a fresh instance for the real, padded processing run, since the calibration instance's internal buffer already consumed `calib` unpadded).

- [ ] **Step 3: Run the M1 regression tests**

Run: `cargo test -p manta-engine --test listen_audio -- --nocapture`
Expected: PASS (`listen_decodes_a_clean_real_audio_signal` still finds "W1AW" in the decoded text).

Run: `cargo test -p manta-engine soak:: -- --nocapture` and `cargo test -p manta-cli --test soak_ci -- --nocapture`
Expected: PASS (both exercise `listen()` under sustained operation; confirm no panic/hang from the channelizer swap).

- [ ] **Step 4: Add a channelizer-specific chunking-determinism test**

M1's Task 6 (`crates/manta-engine/tests/chunking_determinism.rs`) proves the OLD `SingleChannelExtractor`/`TrackDecoder` pairing gives identical output whether fed in one call or many small chunks -- leave that test as-is (it still validates the deprecated path, which is still compiled and tested per Task 4). Add the same proof for the NEW channelizer + placeholder-detector pairing:

Create `crates/manta-engine/tests/channelizer_chunking_determinism.rs`:

```rust
//! Proves the channelizer + placeholder-detector pairing (Tasks 1-6) is
//! chunk-size-invariant, the same property M1's chunking_determinism.rs
//! proved for the deprecated SingleChannelExtractor path.

use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::channelizer::Channelizer;
use manta_testkit::vectors::{render, v1};

fn decode_all_at_once(iq: &[num_complex::Complex32], fs: f64, k0: usize) -> Vec<DecoderEvent> {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for hop_out in ch.process(iq) {
        events.extend(decoder.push_envelope(hop_out.power[k0].sqrt(), hop_out.m * ch.hop() as u64));
    }
    events.extend(decoder.finish());
    events
}

fn decode_in_chunks(
    iq: &[num_complex::Complex32],
    fs: f64,
    k0: usize,
    chunk_size: usize,
) -> Vec<DecoderEvent> {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let hop = ch.hop() as u64;
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for chunk in iq.chunks(chunk_size) {
        for hop_out in ch.process(chunk) {
            events.extend(decoder.push_envelope(hop_out.power[k0].sqrt(), hop_out.m * hop));
        }
    }
    events.extend(decoder.finish());
    events
}

#[test]
fn chunked_channelizer_feeding_matches_whole_buffer_feeding() {
    let spec = v1();
    let rendered = render(&spec).unwrap();

    let mut calib_ch = Channelizer::new(spec.fs, 0.0).unwrap();
    let calib_hops = calib_ch.process(&rendered.samples);
    let n = calib_ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in &calib_hops {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    let mut k0 = 0;
    for (k, &p) in avg_power.iter().enumerate() {
        if p > avg_power[k0] {
            k0 = k;
        }
    }

    let whole = decode_all_at_once(&rendered.samples, spec.fs, k0);
    for &chunk_size in &[97usize, 1_024, 8_192, 100_000] {
        let chunked = decode_in_chunks(&rendered.samples, spec.fs, k0, chunk_size);
        assert_eq!(
            whole, chunked,
            "chunk_size={chunk_size} produced different events than whole-buffer decode"
        );
    }
}
```

- [ ] **Step 5: Run the new test**

Run: `cargo test -p manta-engine --test channelizer_chunking_determinism -- --nocapture`
Expected: PASS. If it fails, this is a real bug in `Channelizer::process`'s incremental-call handling (its `buf`/`read`/`m` state should behave identically to `SingleChannelExtractor`'s equivalent fields across multiple calls) -- do not weaken the assertion; investigate the buffer/compaction logic in Task 1's `process` method.

- [ ] **Step 6: Run clippy and full engine suite**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -40`
Run: `cargo test -p manta-engine 2>&1 | tail -60` (confirm no regressions across `listen_audio.rs`, both `chunking_determinism.rs` files, `pipeline.rs`, `roundtrip_iq.rs`, `soak::`)
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add crates/manta-engine/src/listen.rs crates/manta-engine/tests/channelizer_chunking_determinism.rs
git commit -m "feat(engine): wire the channelizer + placeholder detector into listen; prove chunk-size invariance"
```

---

### Task 8: Docs close-out

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Create: `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`

- [ ] **Step 1: Update ARCHITECTURE.md §4**

§4 ("Channelizer") currently describes the PFB as a design decision not yet built. Add a one-line status note at the top of §4 (matching the style already used elsewhere in this doc for implemented-vs-planned sections): "**Implemented** as of M2 sub-project 1 (`manta-dsp::channelizer`) -- the design below is now built, not just decided." Do not otherwise rewrite §4's content; it already accurately describes what was built (this plan implements the design doc's §2, which is itself a direct application of ARCHITECTURE §4 / SPEC §1).

Update the dependency graph / "Reused from coppa vs. new" table if needed -- check whether `manta-dsp::channelizer` introduces any new edge (it doesn't: it depends only on `crate::proto` and `coppa_dsp::fft`, both already-drawn edges).

- [ ] **Step 2: Update ROADMAP.md**

Under M2's bullet list, note that the PFB channelizer sub-project is complete (link to this plan), and that detector/track manager, decoder pool, SoapySDR input, and KiwiSDR input remain. Do not mark M2 itself complete -- only this first sub-project.

- [ ] **Step 3: Update CLAUDE.md's Status line**

Reflect that M2's first sub-project (PFB channelizer) is complete, `manta-dsp::single`/`freqest` are deprecated-in-place, and the remaining M2 sub-projects (detector/track manager, decoder pool, SoapySDR/KiwiSDR input) are next. Keep it to the existing one-line style.

- [ ] **Step 4: Write the pinned-decisions doc**

Create `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`, following `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`'s structure. Record:

1. **Power-to-dB epsilon**: the channelizer uses SPEC §1.3/§1.4's stated `ε = 1e-20`, not `freqest.rs`'s `1e-30` (the M0 shim's own undocumented choice) -- a deliberate, spec-driven divergence from the deprecated code, not an inconsistency to fix.
2. **WOLA fold accumulation**: implemented in `f64` (via `Complex64` intermediates, cast to `Complex32` only after the fold sum completes), matching the project's existing "long accumulations run sequentially in f64" convention (same as `single.rs`'s direct-FIR sum and `proto.rs`'s prototype design) even though the fold's per-bin sum (`L=8` terms) is much shorter than `single.rs`'s full `LN`-term convolution.
3. **`manta-dsp::single`/`freqest` deprecated in place, not deleted** -- per Tony's explicit decision, kept compiled/tested as reference/fallback; candidate for removal after the channelizer path has run cleanly for a few months (this is a real follow-up to schedule later, not an open question here).
4. **Two `Channelizer` instances per calibration+decode run** (one consumed by `calibrate_channel`, a fresh one for the real padded processing pass) in both `decode_samples` and `listen` -- mirrors the existing M0/M1 pattern of a fresh extractor for the padded run; document this rather than trying to "rewind" a single instance's internal buffer state.
5. Note whatever Task 6 Step 4's actual outcome was (fine-frequency interpolator wired in for `freq_hz` reporting, or not needed) -- record the real result, not the plan's contingency language.

- [ ] **Step 5: Full workspace verification**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace 2>&1 | tail -100`
Expected: clean on all three (V5 golden test correctly shows `ignored`, not failing -- unaffected by this plan).

- [ ] **Step 6: Commit, push, open draft PR**

```bash
git add ARCHITECTURE.md ROADMAP.md CLAUDE.md docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md
git commit -m "docs: M2 sub-project 1 close-out - PFB channelizer implemented, pinned decisions"
git push -u origin feat/m2-pfb-channelizer
gh pr create --draft --title "M2 sub-project 1: PFB channelizer" --body "Implements docs/superpowers/plans/2026-07-18-m2-pfb-channelizer.md (design: docs/superpowers/specs/2026-07-18-m2-pfb-channelizer-design.md). V1-V4/V6 pass through the new channelizer + placeholder detector; V5 unaffected (still #[ignore]d per M1). manta-dsp::single/freqest deprecated in place, not deleted. Detector/track manager, decoder pool, and SoapySDR/KiwiSDR input remain as separate M2 sub-projects."
```

---

## Post-plan check

Before considering this sub-project done: all 8 tasks' tests green, `cargo test --workspace` clean (V5 `ignored`, everything else passing), full-workspace clippy clean, and the pinned-decisions doc accurately reflects what was actually built (not the plan's contingency language) -- especially Task 6 Step 4's fine-frequency-interpolator outcome.
