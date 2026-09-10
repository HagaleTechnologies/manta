# Decode Core v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace manta's threshold → runs → 2-means → beam decode chain with an edge-centered evidence front end and a hidden semi-Markov (HSMM) token-passing decoder, measured against real contest CW (the B2 recording scored against K5TR's own skimmer spots), while keeping the legacy chain selectable until the new engine wins every gate.

**Architecture:** Stage 0 builds the measurement harness (K5TR-only scoring, a decode-core oracle that bypasses the tracker). Stage 1 adds a per-track noise reference and an evidence front end whose decision surface is the half-amplitude crossing, first feeding the *legacy* timing/beam through a new `EdgeLegacy` engine. Stage 2 adds the `Hsmm` engine (tokens over Morse-tree nodes with per-token speed, segment durations measured anchor-to-anchor, partial-traceback commit, posterior confidence and alternatives). Stage 3 adds the optional narrowband refiner and the confidence-separation report. Every stage is gated on the oracle numbers and V1–V10.

**Tech Stack:** Rust workspace (edition 2021), `serde`, `criterion`, `proptest`, `clap`; Python 3 (stdlib only) for `scripts/`; existing crates `manta-dsp`, `manta-decode`, `manta-testkit`, `manta-input`, `manta-cli`.

**Spec:** `docs/SPEC-decode-core-v2.md` (normative) and `docs/superpowers/specs/2026-09-09-decode-core-real-hf-design.md` (rationale). Read both first. `docs/SPEC-decode-core.md` (v1) stays in force for everything v2 does not supersede.

## Global Constraints

- Determinism (v1 §6, v2 §6): no RNG, no wall clock, no `HashMap` iteration in output-affecting paths, `f64` sequential prefix sums, `f32` elsewhere with fixed operation order, ties broken by v2 §6.2's key with `total_cmp`.
- `DecoderEvent` changes are additive only; new fields carry `#[serde(default)]` (v2 §5).
- Do **not** modify `crates/manta-engine/src/track.rs`, `crates/manta-spot/src/gate.rs`, or `crates/manta-spot/src/validator.rs` (owned by parallel work). The one engine call site this plan needs (Task 13) is isolated and sequenced last.
- Clean room: implement from the spec; do not consult or transcribe other decoders' source.
- Every constant is a config key with the v2 §7 name and default.
- Golden vectors V1–V10 must keep passing with `engine = "legacy"` after every task.
- Commit messages: conventional commits with scope (`feat(manta-decode): …`); no AI co-author trailer.
- Run `cargo test --workspace` before declaring any task done; run `cargo clippy --workspace --all-targets -- -D warnings` before each commit.
- Real-audio files under `crates/manta-testkit/audio-corpus/` are local-only and never committed.

---

## File structure

| path | responsibility |
|---|---|
| `scripts/score-against-rbn.py` (modify) | `--spotter` / `--min-spotters` truth filters |
| `scripts/tests/test_score_against_rbn.py` (create) | scorer tests (pytest, stdlib data) |
| `crates/manta-decode/src/events.rs` (modify) | additive `alternatives`, `confidence` fields |
| `crates/manta-dsp/src/floor.rs` (modify) | `FloorBank::spectral_reference_db` (v2 §2.2) |
| `crates/manta-decode/src/noise.rs` (create) | `NoiseTracker`: temporal min statistics + combination (v2 §2.1, §2.3) |
| `crates/manta-decode/src/evidence.rs` (create) | `Evidence`: smoothing, centered max, gate, `u_amp`, `llr`, anchors, prefix sums (v2 §1) |
| `crates/manta-decode/src/edge_demod.rs` (create) | `EdgeDemod`: runs from `llr` sign changes for the `EdgeLegacy` engine |
| `crates/manta-decode/src/hsmm/segments.rs` (create) | `SegType`, duration/type priors (v2 §4.2–4.4) |
| `crates/manta-decode/src/hsmm/token.rs` (create) | `Token`, `HistEntry`, ordering, successor construction (v2 §4.5, §6.2) |
| `crates/manta-decode/src/hsmm/mod.rs` (create) | `HsmmDecoder`: anchors, step, merge/prune, seeding, commit, confidence (v2 §4.6–4.10) |
| `crates/manta-decode/src/decoder.rs` (modify) | `Engine` enum, dispatch, `push_hop` |
| `crates/manta-decode/src/lib.rs` (modify) | module declarations |
| `crates/manta-decode/benches/hsmm_300_tracks.rs` (create) | criterion budget bench |
| `crates/manta-testkit/src/oracle.rs` (create) | real-signal decode oracle (v2 §8.3) |
| `crates/manta-testkit/src/keyer.rs`, `scene.rs`, `vectors.rs` (modify) | weighting, gap units, co-channel, VR1–VR8 |
| `crates/manta-cli/src/main.rs` (modify) | `--engine`, `oracle` subcommand, `gen vr*` |
| `crates/manta-cli/tests/golden_vr.rs` (create) | VR1–VR8 gates |
| `crates/manta-cli/tests/determinism_hsmm.rs` (create) | 3-run SHA-256 with `--engine hsmm` |
| `crates/manta-dsp/src/refine.rs` (create) | `Refiner` (v2 §3) |
| `scripts/confidence-separation.py` (create) | MAN-106 separation report |
| `docs/DECISIONS/2026-09-XX-decode-core-v2-stage{1,2}-gate.md` (create at gate time) | measured gate results |

---

### Task 1: K5TR-only scoring in the RBN scorer

**Files:**
- Modify: `scripts/score-against-rbn.py`
- Create: `scripts/tests/test_score_against_rbn.py`

**Interfaces:**
- Produces: CLI flags `--spotter CALL` (repeatable) and `--min-spotters N`; `load_rbn_truth(path, spotters=None, min_spotters=1)`.

- [ ] **Step 1: Write the failing test**

```python
# scripts/tests/test_score_against_rbn.py
import csv, importlib.util, json, os, sys, tempfile
HERE = os.path.dirname(__file__)
spec = importlib.util.spec_from_file_location("scorer", os.path.join(HERE, "..", "score-against-rbn.py"))
scorer = importlib.util.module_from_spec(spec); spec.loader.exec_module(scorer)

ROWS = [
    # callsign(spotter), freq kHz, dx
    ("K5TR", "7021.0", "N4RV"), ("DL1ABC", "7021.0", "N4RV"),
    ("DL1ABC", "7032.0", "9A2L"), ("K5TR", "7049.4", "N3RS"),
]
def write_csv(path):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["callsign","de_pfx","de_cont","freq","band","dx","dx_pfx","dx_cont","mode","db","date","speed","tx_mode"])
        for sp, fq, dx in ROWS:
            w.writerow([sp,"","",fq,"40m",dx,"","","CQ","20","2025-11-29 00:00:00","33","CW"])

def test_spotter_filter_keeps_only_that_spotters_rows():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth = scorer.load_rbn_truth(p, spotters={"K5TR"})
        assert sorted(t["callsign"] for t in truth) == ["N3RS", "N4RV"]

def test_min_spotters_requires_agreement():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth = scorer.load_rbn_truth(p, min_spotters=2)
        assert [t["callsign"] for t in truth] == ["N4RV", "N4RV"]

def test_default_is_unfiltered():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        assert len(scorer.load_rbn_truth(p)) == 4
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -m pytest scripts/tests -q`
Expected: FAIL with `TypeError: load_rbn_truth() got an unexpected keyword argument 'spotters'`

- [ ] **Step 3: Implement the filters**

Replace `load_rbn_truth` in `scripts/score-against-rbn.py`:

```python
def load_rbn_truth(path, spotters=None, min_spotters=1):
    """RBN daily-dump rows -> [{callsign, freq_hz}], optionally restricted
    to rows from `spotters` (a set of spotter callsigns) and to call+kHz
    bins heard by at least `min_spotters` distinct spotters."""
    rows = []
    with open(path) as f:
        for row in csv.DictReader(f):
            if spotters and row["callsign"].upper() not in spotters:
                continue
            rows.append({
                "spotter": row["callsign"].upper(),
                "callsign": row["dx"].upper(),
                "freq_hz": float(row["freq"]) * 1000.0,
            })
    if min_spotters > 1:
        heard = {}
        for r in rows:
            heard.setdefault((r["callsign"], round(r["freq_hz"] / 1000.0)), set()).add(r["spotter"])
        rows = [r for r in rows if len(heard[(r["callsign"], round(r["freq_hz"] / 1000.0))]) >= min_spotters]
    return rows
```

Add the flags in `main()` and pass them through:

```python
    ap.add_argument("--spotter", action="append", default=None,
                    help="Only use truth rows from this spotter callsign (repeatable), e.g. --spotter K5TR for the co-located reference")
    ap.add_argument("--min-spotters", type=int, default=1,
                    help="Only count call+kHz bins heard by at least N distinct spotters")
    ...
    spotters = {s.upper() for s in args.spotter} if args.spotter else None
    truth = load_rbn_truth(args.rbn_truth_csv, spotters=spotters, min_spotters=args.min_spotters)
```

And print the filter in the header: `print(f"truth filter: spotters={sorted(spotters) if spotters else 'all'} min_spotters={args.min_spotters}")`.

- [ ] **Step 4: Run tests**

Run: `python3 -m pytest scripts/tests -q`
Expected: 3 passed

- [ ] **Step 5: Document and commit**

Add to `crates/manta-testkit/audio-corpus/README.md` under "Ground truth": the K5TR-only invocation (`--spotter K5TR`) and the 2026-09-09 baseline (55/210 = 26.2 % recall, 24.3 % precision vs K5TR; 68/1451 = 4.7 %, 30.1 % vs all).

```bash
git add scripts/score-against-rbn.py scripts/tests/test_score_against_rbn.py crates/manta-testkit/audio-corpus/README.md
git commit -m "feat(scripts): score RBN benchmark against a single spotter (K5TR parity metric)"
```

---

### Task 2: Additive event fields

**Files:**
- Modify: `crates/manta-decode/src/events.rs`
- Test: `crates/manta-decode/src/events.rs` (unit tests)

**Interfaces:**
- Produces: `DecoderEvent::CharDecoded { …, alternatives: Vec<(Glyph, f32)> }`, `DecoderEvent::WordBoundary { …, confidence: f32 }`; helper `DecoderEvent::char_decoded(track_id, sample_ts, glyph, confidence)` and `DecoderEvent::word_boundary(track_id, sample_ts)` constructors that fill the defaults (every existing construction site in `decoder.rs` and tests switches to them).

Scope note (design doc §8.3, MAN-167): this task adds the field to the wire type only. It does **not** add a `manta-spot` consumer or a dispensa JSON Schema change — `alternatives` stays internal to `manta-decode`'s event stream until MAN-167 is picked up.

- [ ] **Step 1: Write the failing tests**

```rust
// append to crates/manta-decode/src/events.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Glyph;

    #[test]
    fn legacy_json_without_new_fields_still_deserializes() {
        let json = r#"{"event":"CharDecoded","track_id":1,"sample_ts":10,"glyph":{"Char":"A"},"confidence":0.5}"#;
        let e: DecoderEvent = serde_json::from_str(json).unwrap();
        match e {
            DecoderEvent::CharDecoded { alternatives, .. } => assert!(alternatives.is_empty()),
            _ => panic!(),
        }
        let json = r#"{"event":"WordBoundary","track_id":1,"sample_ts":10}"#;
        let e: DecoderEvent = serde_json::from_str(json).unwrap();
        match e {
            DecoderEvent::WordBoundary { confidence, .. } => assert_eq!(confidence, 1.0),
            _ => panic!(),
        }
    }

    #[test]
    fn constructors_fill_defaults() {
        let e = DecoderEvent::char_decoded(1, 10, Glyph::Char('A'), 0.5);
        assert_eq!(serde_json::to_string(&e).unwrap(),
            r#"{"event":"CharDecoded","track_id":1,"sample_ts":10,"glyph":{"Char":"A"},"confidence":0.5,"alternatives":[]}"#);
        let e = DecoderEvent::word_boundary(1, 10);
        assert_eq!(serde_json::to_string(&e).unwrap(),
            r#"{"event":"WordBoundary","track_id":1,"sample_ts":10,"confidence":1.0}"#);
    }
}
```

`serde_json` and `Deserialize` are needed: add `serde_json = { workspace = true }` under `[dev-dependencies]` in `crates/manta-decode/Cargo.toml`, and derive `serde::Deserialize` on `DecoderEvent`, `Glyph`, `Prosign` (add `serde::Deserialize` to their derive lists in `tree.rs`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-decode events::`
Expected: compile error (no field `alternatives`, no `char_decoded`).

- [ ] **Step 3: Implement**

```rust
// events.rs — replace the two variants and add constructors
    CharDecoded {
        track_id: u32,
        sample_ts: u64,
        glyph: Glyph,
        confidence: f32,
        /// v2 §4.9: up to two competing glyphs at this position with their
        /// posterior mass. Empty for the legacy engine.
        #[serde(default)]
        alternatives: Vec<(Glyph, f32)>,
    },
    WordBoundary {
        track_id: u32,
        sample_ts: u64,
        /// v2 §4.9: posterior that this gap is a word gap. 1.0 for legacy.
        #[serde(default = "one")]
        confidence: f32,
    },
```

```rust
fn one() -> f32 { 1.0 }

impl DecoderEvent {
    pub fn char_decoded(track_id: u32, sample_ts: u64, glyph: Glyph, confidence: f32) -> Self {
        DecoderEvent::CharDecoded { track_id, sample_ts, glyph, confidence, alternatives: Vec::new() }
    }
    pub fn word_boundary(track_id: u32, sample_ts: u64) -> Self {
        DecoderEvent::WordBoundary { track_id, sample_ts, confidence: 1.0 }
    }
}
```

Add `serde::Deserialize` to the derive on `DecoderEvent`. Update every construction site (`decoder.rs` `emit_char`, `check_flush`, `process_run`, `finish`; the `text_assembly_drops_prosigns_and_collapses_spaces` test; any in `manta-engine`, `manta-spot` tests — `grep -rn "DecoderEvent::CharDecoded {" crates` and `grep -rn "DecoderEvent::WordBoundary {" crates`) to the constructors or to include the new fields. Pattern matches with `..` need no change.

- [ ] **Step 4: Run the whole workspace**

Run: `cargo test --workspace`
Expected: all pass (the JSON golden fixtures in `manta-cli/tests` compare text/spots, not raw event JSON; if any compares event JSON byte-for-byte, regenerate it and note it in the commit).

- [ ] **Step 5: Commit**

```bash
git add crates/manta-decode crates/manta-engine crates/manta-spot
git commit -m "feat(manta-decode): additive alternatives/confidence fields on decoder events (SPEC v2 §5)"
```

---

### Task 3: Track-local noise reference

**Files:**
- Modify: `crates/manta-dsp/src/floor.rs`
- Create: `crates/manta-decode/src/noise.rs`
- Modify: `crates/manta-decode/src/lib.rs` (add `pub mod noise;`)

**Interfaces:**
- Produces: `FloorBank::spectral_reference_db(&self, c: usize) -> f64` (min of the gate-EMA smoothed power over `c±2..c±4`, in dB, **uncorrected**); `manta_decode::noise::NoiseTracker::new(cfg: NoiseConfig)`, `NoiseTracker::push(&mut self, power: f32, spectral_ref_power: Option<f32>) -> f32` returning `N[t]` in linear power (v2 §2.3).
- Consumes: nothing new.

- [ ] **Step 1: Failing test for the spectral reference (manta-dsp)**

`FloorBank` today stores decimated ring histograms, not the 40 ms EMA; the EMA lives in `Gate`. Move a per-channel `smoothed_db` EMA into `FloorBank::update` (same `GATE_EMA_ALPHA`, updated every hop, not decimated) so the reference is per hop; `Gate` may keep its own copy (do not change `Gate`'s public API).

```rust
// floor.rs tests
    #[test]
    fn spectral_reference_is_min_over_guard_banded_neighbors() {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[10] = -30.0;        // the track itself
        p[11] = -40.0;        // guard cell, must be ignored
        p[12] = -70.0; p[13] = -85.0; p[14] = -80.0;   // c+2..c+4
        p[8] = -60.0;  p[7] = -95.0;  p[6] = -75.0;    // c-2..c-4
        for _ in 0..400 { bank.update(&p); }           // let the EMA settle
        let r = bank.spectral_reference_db(10);
        assert!((r - -95.0).abs() < 0.2, "expected min over {{6,7,8,12,13,14}} = -95 dB, got {r}");
    }

    #[test]
    fn spectral_reference_wraps_at_band_edges() {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[62] = -99.0;    // c = 0 -> c-2 wraps to 62
        for _ in 0..400 { bank.update(&p); }
        assert!((bank.spectral_reference_db(0) - -99.0).abs() < 0.2);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-dsp spectral_reference`
Expected: compile error (no method `spectral_reference_db`).

- [ ] **Step 3: Implement**

```rust
// floor.rs: add to FloorBank
    /// Per-hop EMA (tau = 40 ms, same alpha as Gate) of each channel's dB
    /// power, kept here so the decoder's spectral noise reference (SPEC v2
    /// §2.2) needs no extra per-channel state.
    smoothed_db: Vec<f64>,
    smoothed_init: bool,
```

In `new`: `smoothed_db: vec![HIST_MIN_DB; n_channels], smoothed_init: false`. In `update`, before the decimation check:

```rust
        if self.smoothed_init {
            for (s, &p) in self.smoothed_db.iter_mut().zip(power_db) {
                *s += GATE_EMA_ALPHA * (p - *s);
            }
        } else {
            self.smoothed_db.copy_from_slice(power_db);
            self.smoothed_init = true;
        }
```

(`GATE_EMA_ALPHA` is defined below `FloorBank` in the file; move the constant above it.) Then:

```rust
    /// SPEC v2 §2.2: minimum of the 40 ms-smoothed power over the six
    /// guard-banded neighbor channels c±2..c±4 (wrapping), in dB,
    /// uncorrected for the min-of-six bias (the decoder applies it).
    pub fn spectral_reference_db(&self, c: usize) -> f64 {
        let n = self.smoothed_db.len() as isize;
        let mut m = f64::INFINITY;
        for d in [-4isize, -3, -2, 2, 3, 4] {
            let k = (c as isize + d).rem_euclid(n) as usize;
            m = m.min(self.smoothed_db[k]);
        }
        m
    }
```

- [ ] **Step 4: Failing tests for `NoiseTracker` (manta-decode)**

```rust
// crates/manta-decode/src/noise.rs
//! Track-local noise reference: temporal minimum statistics combined with
//! a spectral (guard-banded neighbor) reference. SPEC v2 §2.

use crate::{ms_to_hops, HOP_MS};
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct NoiseConfig {
    pub noise_window_ms: f64,
    pub noise_min_bias_db: f32,
    pub spectral_min_bias_db: f32,
    pub spectral_beta: f32,
    pub tau_ms: f64,
}

impl Default for NoiseConfig {
    fn default() -> Self {
        NoiseConfig { noise_window_ms: 1500.0, noise_min_bias_db: 2.5, spectral_min_bias_db: 1.5, spectral_beta: 0.5, tau_ms: 40.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic exponential (chi-square 2 DOF) noise power samples via a
    /// fixed LCG — a test-only RNG, allowed by SPEC v1 §6.1 (seeds are part
    /// of the test).
    fn exp_noise(n: usize, mean: f32, seed: u64) -> Vec<f32> {
        let mut s = seed; let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u = ((s >> 11) as f64 / (1u64 << 53) as f64).max(1e-12);
            out.push((-u.ln() as f32) * mean);
        }
        out
    }

    #[test]
    fn temporal_minimum_is_unbiased_on_noise_within_half_db() {
        let mut t = NoiseTracker::new(NoiseConfig::default());
        let x = exp_noise(375 * 60, 1.0e-6, 7);
        let mut acc = 0.0f64; let mut n = 0usize;
        for (i, &p) in x.iter().enumerate() {
            let v = t.push(p, None);
            if i > 375 * 5 { acc += v as f64; n += 1; }
        }
        let est_db = 10.0 * (acc / n as f64 / 1.0e-6).log10();
        assert!(est_db.abs() < 0.5, "temporal noise estimate off by {est_db:.2} dB");
    }

    #[test]
    fn spectral_reference_raises_the_floor_during_a_burst() {
        let mut t = NoiseTracker::new(NoiseConfig::default());
        for _ in 0..375 * 5 { t.push(1.0e-6, None); }
        let quiet = t.push(1.0e-6, Some(1.0e-6));
        let burst = t.push(1.0e-6, Some(1.0e-3));   // neighbors 30 dB up
        assert!(burst > 100.0 * quiet, "spectral burst must lift N: quiet {quiet:e} burst {burst:e}");
    }
}
```

- [ ] **Step 5: Implement `NoiseTracker`**

```rust
pub struct NoiseTracker {
    cfg: NoiseConfig,
    alpha: f32,
    smoothed: Option<f32>,
    window: usize,
    /// Monotonic deque of (hop, smoothed power): front is the window minimum.
    deque: VecDeque<(u64, f32)>,
    hop: u64,
    b_min: f32,
    b_spec: f32,
}

impl NoiseTracker {
    pub fn new(cfg: NoiseConfig) -> Self {
        let alpha = (1.0 - (-HOP_MS / cfg.tau_ms).exp()) as f32;
        let window = ms_to_hops(cfg.noise_window_ms).max(1) as usize;
        let b_min = 10f32.powf(cfg.noise_min_bias_db / 10.0);
        let b_spec = 10f32.powf(cfg.spectral_min_bias_db / 10.0);
        NoiseTracker { cfg, alpha, smoothed: None, window, deque: VecDeque::new(), hop: 0, b_min, b_spec }
    }

    /// One hop of the track's own channel power (linear) and, if available,
    /// the raw spectral reference power (linear, from
    /// `FloorBank::spectral_reference_db` converted out of dB). Returns
    /// `N[t]` in linear power. SPEC v2 §2.3.
    pub fn push(&mut self, power: f32, spectral_ref_power: Option<f32>) -> f32 {
        let s = match self.smoothed {
            None => power,
            Some(prev) => prev + self.alpha * (power - prev),
        };
        self.smoothed = Some(s);
        while let Some(&(_, back)) = self.deque.back() {
            if back >= s { self.deque.pop_back(); } else { break; }
        }
        self.deque.push_back((self.hop, s));
        while let Some(&(h, _)) = self.deque.front() {
            if self.hop - h >= self.window as u64 { self.deque.pop_front(); } else { break; }
        }
        self.hop += 1;
        let n_temp = self.deque.front().map(|&(_, v)| v).unwrap_or(s) * self.b_min;
        match spectral_ref_power {
            Some(r) => n_temp.max(self.cfg.spectral_beta * self.b_spec * r),
            None => n_temp,
        }
    }
}
```

- [ ] **Step 6: Calibrate `noise_min_bias_db`**

Run: `cargo test -p manta-decode noise::` — if `temporal_minimum_is_unbiased_on_noise_within_half_db` fails, print the measured offset (add a temporary `eprintln!`) and set `NoiseConfig::default().noise_min_bias_db` to the value that zeroes it (expected ≈ 2–3 dB for a 40 ms EMA and 1.5 s window); record the measured value in the doc comment and in `docs/SPEC-decode-core-v2.md` §2.1. Re-run until it passes.

- [ ] **Step 7: Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/manta-dsp/src/floor.rs crates/manta-decode/src/noise.rs crates/manta-decode/src/lib.rs docs/SPEC-decode-core-v2.md
git commit -m "feat(manta-decode): track-local noise reference (temporal minimum statistics + spectral CFAR) (SPEC v2 §2)"
```

---

### Task 4: Evidence front end

**Files:**
- Create: `crates/manta-decode/src/evidence.rs`
- Modify: `crates/manta-decode/src/lib.rs` (add `pub mod evidence;`)

**Interfaces:**
- Produces:

```rust
pub struct EvidenceConfig { pub sigma_u: f32, pub llr_clip: f32, pub hold_dits: f32, pub fallback_hops: u32, pub tau_a_ms: f64, pub u_init_hops: f32 }
pub struct HopEvidence { pub hop: u64, pub sample_ts: u64, pub llr: f32, pub present: bool, pub anchor: bool, pub prefix: f64, pub amp: f32, pub mark_level: f32, pub noise_level: f32 }
impl Evidence {
    pub fn new(cfg: EvidenceConfig) -> Self;
    pub fn push(&mut self, amp: f32, noise_amp: f32, sample_ts: u64) -> Option<HopEvidence>;  // None during the h-hop delay
    pub fn flush(&mut self) -> Vec<HopEvidence>;   // drain the delay line at end of stream
    pub fn set_u_ref(&mut self, u_hops: f32);      // resizes h = round(hold_dits * u)
}
```

`prefix` is `C[t]` (v2 §1.8), accumulated in `f64` **over the emitted hops** so any consumer can compute `C[t] − C[s]`.

- [ ] **Step 1: Failing tests**

```rust
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
        let mut e = Evidence::new(EvidenceConfig::default());
        let mut out = Vec::new();
        for (i, &a) in env.iter().enumerate() { if let Some(h) = e.push(a, noise_amp, i as u64 * 512) { out.push(h); } }
        out.extend(e.flush());
        out
    }

    #[test]
    fn llr_is_antisymmetric_about_half_amplitude() {
        let mut e = Evidence::new(EvidenceConfig::default());
        for _ in 0..200 { e.push(1.0, 0.01, 0); }          // establish M = 1
        let hi = (0..200).filter_map(|_| e.push(0.75, 0.01, 0)).last().unwrap().llr;
        let lo = (0..200).filter_map(|_| e.push(0.25, 0.01, 0)).last().unwrap().llr;
        assert!((hi + lo).abs() < 0.05, "hi {hi} lo {lo}");
        assert!(hi > 0.0);
    }

    #[test]
    fn anchors_land_on_true_edges_regardless_of_depth() {
        for depth in [20.0f32, 40.0, 60.0] {
            let env = keyed(13, depth, 20);
            let hops = run(&env, 10f32.powf(-depth / 20.0));
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
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-decode evidence::`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
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
        let mut out = Vec::new();
        while self.h > 0 && !self.line.is_empty() {
            self.h -= 1;
            if self.line.len() > 2 * self.h + 1 { self.line.pop_front(); }
            if self.line.len() < self.h + 1 { break; }
            let center = self.line.len() - 1 - self.h;
            out.push(self.emit(center));
        }
        out
    }

    fn emit(&mut self, center: usize) -> HopEvidence {
        let (amp, _, n, ts) = self.line[center];
        // Centered maximum of the smoothed amplitude over [center-h, center+h]
        // (linear scan; window <= 2*hold_dits*56+1, deterministic).
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
        let anchor = sign != self.last_sign
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
```

- [ ] **Step 4: Run tests; tune nothing — fix bugs only**

Run: `cargo test -p manta-decode evidence::`
Expected: 5 passed. If `anchors_land_on_true_edges_regardless_of_depth` fails, the bug is in the delay/center indexing, not in the constants.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-decode/src/evidence.rs crates/manta-decode/src/lib.rs
git commit -m "feat(manta-decode): half-amplitude-centered evidence front end with anchors (SPEC v2 §1)"
```

---

### Task 5: `EdgeLegacy` engine (stage 1 behind the legacy chain)

**Files:**
- Create: `crates/manta-decode/src/edge_demod.rs`
- Modify: `crates/manta-decode/src/decoder.rs`
- Modify: `crates/manta-decode/src/lib.rs`

**Interfaces:**
- Produces: `pub enum Engine { Legacy, EdgeLegacy, Hsmm }` on `DecodeConfig` (`engine: Engine`, default `Legacy`, `serde` + `clap::ValueEnum`-friendly `FromStr`); `DecodeConfig.evidence: EvidenceConfig`, `DecodeConfig.noise: NoiseConfig`; `TrackDecoder::push_hop(&mut self, amp: f32, power: f32, spectral_ref_power: Option<f32>, sample_ts: u64) -> Vec<DecoderEvent>`; `push_envelope(a, ts)` keeps working and calls `push_hop(a, a*a, None, ts)`.
- `EdgeDemod::push(&mut self, ev: &HopEvidence) -> Vec<Run>` (same `Run` type as `envelope.rs`) turning `llr` sign runs into mark/space runs with the 12 ms debounce; `EdgeDemod::finish()`.

- [ ] **Step 1: Failing test — the stage-1 acceptance shape**

```rust
// decoder.rs tests, add:
    /// Rectangular envelope helper with `depth_db` keying depth (the existing
    /// `rect_envelope` is 0/1; deep keying is what breaks the legacy chain).
    fn rect_envelope_depth(text: &str, dit_hops: u32, depth_db: f32) -> Vec<f32> {
        let lo = 10f32.powf(-depth_db / 20.0);
        rect_envelope(text, dit_hops).into_iter().map(|v| if v > 0.5 { 1.0 } else { lo }).collect()
    }

    fn decode_with(engine: Engine, env: &[f32]) -> String {
        let mut cfg = DecodeConfig::default();
        cfg.engine = engine;
        let mut dec = TrackDecoder::new(1, cfg);
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() { events.extend(dec.push_envelope(a, i as u64 * 256)); }
        events.extend(dec.finish());
        events_to_text(&events)
    }

    #[test]
    fn edge_legacy_decodes_contest_speed_deep_keying() {
        // 40 WPM = 11.25 hops/dit; use 11 hops. 45 dB depth. Legacy merges words here.
        let env = rect_envelope_depth("CQ TEST W5AU W5AU TEST", 11, 45.0);
        assert_eq!(decode_with(Engine::EdgeLegacy, &env), "CQ TEST W5AU W5AU TEST");
    }

    #[test]
    fn legacy_engine_is_untouched() {
        let env = rect_envelope("CQ CQ DE W1AW", 18);
        assert_eq!(decode_with(Engine::Legacy, &env), "CQ CQ DE W1AW");
    }
```

Note: with a *rectangular* synthetic envelope the legacy chain may pass the first test too (the bias comes from filter-smeared edges). To make the test meaningful, apply the same 4-hop ramp as Task 4's `keyed()` helper to `rect_envelope_depth`'s output (copy that ramp loop). With the ramp and 45 dB depth, `Engine::Legacy` produces `CQTESTW5AU…` — add an assertion documenting that: `assert_ne!(decode_with(Engine::Legacy, &env), "CQ TEST W5AU W5AU TEST")` (if it unexpectedly passes, raise the depth to 60 dB and the speed to 9 hops before weakening the assertion).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-decode edge_legacy`
Expected: compile error (`Engine` missing).

- [ ] **Step 3: Implement `EdgeDemod`**

```rust
//! Runs from evidence sign changes, for the EdgeLegacy engine (stage 1):
//! the legacy timing/beam chain fed by SPEC v2 §1's half-amplitude
//! decision instead of §3.2's geometric-mean threshold.

use crate::envelope::Run;
use crate::evidence::HopEvidence;
use crate::ms_to_hops;

pub struct EdgeDemod {
    debounce_hops: u32,
    open: Option<Run>,
    held: Option<Run>,
}

impl EdgeDemod {
    pub fn new(debounce_ms: f64) -> Self {
        EdgeDemod { debounce_hops: ms_to_hops(debounce_ms), open: None, held: None }
    }

    pub fn push(&mut self, ev: &HopEvidence) -> Vec<Run> {
        let mark = ev.llr > 0.0;
        let mut out = Vec::new();
        match self.open {
            None => self.open = Some(Run { mark, start_ts: ev.sample_ts, hops: 1 }),
            Some(ref mut o) if o.mark == mark => o.hops += 1,
            Some(o) => {
                if o.hops < self.debounce_hops {
                    // Short run: merge held + short + continuing into held's polarity
                    // (same rule as envelope.rs, SPEC v1 §3.3).
                    match self.held.take() {
                        Some(h) => self.open = Some(Run { mark: h.mark, start_ts: h.start_ts, hops: h.hops + o.hops + 1 }),
                        None => self.open = Some(Run { mark, start_ts: o.start_ts, hops: o.hops + 1 }),
                    }
                } else {
                    if let Some(h) = self.held.take() { out.push(h); }
                    self.held = Some(o);
                    self.open = Some(Run { mark, start_ts: ev.sample_ts, hops: 1 });
                }
            }
        }
        out
    }

    pub fn open_space(&self) -> Option<(u32, u64)> {
        match self.open { Some(r) if !r.mark => Some((r.hops, r.start_ts)), _ => None }
    }

    pub fn finish(&mut self) -> Vec<Run> {
        let mut out = Vec::new();
        if let Some(o) = self.open.take() {
            if o.hops >= self.debounce_hops { if let Some(h) = self.held.take() { out.push(h); } out.push(o); }
            else if let Some(mut h) = self.held.take() { h.hops += o.hops; out.push(h); }
        } else if let Some(h) = self.held.take() { out.push(h); }
        out
    }
}
```

- [ ] **Step 4: Wire `Engine` into `TrackDecoder`**

In `decoder.rs`:

```rust
use crate::edge_demod::EdgeDemod;
use crate::evidence::{Evidence, EvidenceConfig, HopEvidence};
use crate::noise::{NoiseConfig, NoiseTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine { Legacy, EdgeLegacy, Hsmm }

impl std::str::FromStr for Engine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s { "legacy" => Ok(Engine::Legacy), "edge-legacy" => Ok(Engine::EdgeLegacy), "hsmm" => Ok(Engine::Hsmm), o => Err(format!("unknown decode engine {o:?} (legacy | edge-legacy | hsmm)")) }
    }
}
```

Add to `DecodeConfig`: `pub engine: Engine, pub evidence: EvidenceConfig, pub noise: NoiseConfig` (defaults `Legacy`, `EvidenceConfig::default()`, `NoiseConfig::default()`). Add to `TrackDecoder`: `evidence: Evidence, noise: NoiseTracker, edge: EdgeDemod`, constructed in `new`. Add:

```rust
    /// One hop: linear amplitude, linear power, optional spectral noise
    /// reference power (SPEC v2 §2.2), input sample counter.
    pub fn push_hop(&mut self, amp: f32, power: f32, spectral_ref_power: Option<f32>, sample_ts: u64) -> Vec<DecoderEvent> {
        match self.cfg.engine {
            Engine::Legacy => self.push_envelope_legacy(amp, sample_ts),
            Engine::EdgeLegacy => {
                let n = self.noise.push(power, spectral_ref_power).max(1e-18).sqrt();
                let mut events = Vec::new();
                if let Some(ev) = self.evidence.push(amp, n, sample_ts) {
                    self.snr_from_evidence(&ev);
                    for run in self.edge.push(&ev) { self.on_run(run, &mut events); }
                    self.check_flush_edge(&mut events);
                }
                self.tick_meta(&mut events);
                events
            }
            Engine::Hsmm => self.push_hop_hsmm(amp, power, spectral_ref_power, sample_ts), // Task 8
        }
    }

    pub fn push_envelope(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent> {
        self.push_hop(a, a * a, None, sample_ts)
    }
```

Refactor the existing `push_envelope` body into `push_envelope_legacy` (unchanged logic) and extract the `TrackMeta` cadence into `tick_meta(&mut self, events)` (increments `hop_count`, emits at `META_INTERVAL_HOPS`). `snr_from_evidence` stores `20·log10(mark_level/noise_level) − 14.3` into a `last_snr: Option<f32>` field used by `emit_char`'s `q` and by `tick_meta` when the engine is not `Legacy` (legacy keeps `demod.snr_2500_db()`). `check_flush_edge` is `check_flush` with `self.edge.open_space()` in place of the two `demod.open_space_*` calls and `self.edge.finish()` in place of `self.demod.finish()`. In `finish()`, drain `self.evidence.flush()` through `self.edge` when the engine is `EdgeLegacy` before the existing flush logic. Until Task 8, `push_hop_hsmm` is `unimplemented!("Task 8")` and `Engine::Hsmm` is not reachable from any test.

- [ ] **Step 5: Run tests**

Run: `cargo test -p manta-decode`
Expected: all pass, including `edge_legacy_decodes_contest_speed_deep_keying` and every pre-existing test. Then `cargo test --workspace` (V1–V10 with the default `Legacy` engine untouched).

- [ ] **Step 6: Commit**

```bash
git add crates/manta-decode
git commit -m "feat(manta-decode): EdgeLegacy engine — legacy timing/beam fed by the half-amplitude evidence decision (stage 1)"
```

---

### Task 6: Real-signal decode oracle + CLI

**Files:**
- Create: `crates/manta-testkit/src/oracle.rs`
- Modify: `crates/manta-testkit/src/lib.rs`, `crates/manta-testkit/Cargo.toml` (`manta-input` moves from dev- to normal dependencies)
- Modify: `crates/manta-cli/src/main.rs` (`oracle` subcommand; `--engine` on `decode` and `oracle`)
- Test: `crates/manta-testkit/tests/oracle_synthetic.rs`

**Interfaces:**
- Produces:

```rust
pub struct OracleSpot { pub call: String, pub khz: f64, pub t_sec: f64, pub snr_db: i32, pub wpm: i32 }
pub struct OracleResult { pub call: String, pub khz: f64, pub as_word: bool, pub framed: bool, pub substring: bool, pub wpm_ratio: Option<f32>, pub text: String }
pub struct OracleSummary { pub n: usize, pub as_word: usize, pub framed: usize, pub substring: usize, pub wpm_ratio_median: Option<f32>, pub by_snr: Vec<(String, usize, usize, usize)> }
pub fn parse_rbn_spots(csv_path: &Path, spotter: &str) -> anyhow::Result<Vec<OracleSpot>>;   // first spot per (call, kHz)
pub fn run_oracle(iq: &[Complex32], fs: f64, center_hz: f64, spots: &[OracleSpot], window_s: f64, cfg: &DecodeConfig) -> anyhow::Result<(Vec<OracleResult>, OracleSummary)>;
pub fn score_text(call: &str, text: &str) -> (bool, bool, bool);   // (as_word, framed, substring)
```

- [ ] **Step 1: Failing tests**

```rust
// crates/manta-testkit/tests/oracle_synthetic.rs
use manta_testkit::oracle::{run_oracle, score_text, OracleSpot};
use manta_testkit::scene::{render_scene, SignalSpec};

#[test]
fn score_text_matches_the_scorer_definitions() {
    assert_eq!(score_text("NJ3K", "CQ TEST NJ3K CQ TEST NJ3K"), (true, true, true));
    assert_eq!(score_text("NJ3K", "CQTESTNJ3K"), (false, false, true));
    assert_eq!(score_text("NJ3K", "TEST NJ3K"), (true, true, true));
    assert_eq!(score_text("NJ3K", "DE NJ3K"), (true, true, true));
    assert_eq!(score_text("NJ3K", "NJ3K"), (true, false, true));
    assert_eq!(score_text("NJ3K", "CQ TEST NJ3N"), (false, false, false));
}

#[test]
fn oracle_recovers_a_synthetic_station_at_its_rbn_khz() {
    let sig = SignalSpec { text: "CQ TEST W5AU W5AU TEST".into(), loop_text: true, wpm: 30.0, offset_hz: 12_340.0,
        snr_2500_db: 25.0, jitter: None, qsb: None, watterson: None, char_wpm: None };
    let (iq, _) = render_scene(&[sig], 96_000.0, 60.0, Some(1)).unwrap();
    let spots = vec![OracleSpot { call: "W5AU".into(), khz: 14_012.3, t_sec: 20.0, snr_db: 25, wpm: 30 }];
    let (results, summary) = run_oracle(&iq, 96_000.0, 14_000_000.0, &spots, 40.0, &Default::default()).unwrap();
    assert!(results[0].as_word, "text: {}", results[0].text);
    assert_eq!(summary.as_word, 1);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-testkit --test oracle_synthetic`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
//! Real-signal decode oracle: channelize a window around each RBN-spotted
//! station, feed the strongest of the three channels around its kHz
//! straight into `TrackDecoder`, and score callsign recovery. Isolates
//! the decode core from the tracker. SPEC v2 §8.3.

use anyhow::{Context, Result};
use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::channelizer::Channelizer;
use num_complex::Complex32;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleSpot { pub call: String, pub khz: f64, pub t_sec: f64, pub snr_db: i32, pub wpm: i32 }

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleResult { pub call: String, pub khz: f64, pub as_word: bool, pub framed: bool, pub substring: bool, pub wpm_ratio: Option<f32>, pub text: String }

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleSummary { pub n: usize, pub as_word: usize, pub framed: usize, pub substring: usize, pub wpm_ratio_median: Option<f32>, pub by_snr: Vec<(String, usize, usize, usize)> }

/// First spot per (call, kHz) from an RBN daily-dump CSV, restricted to `spotter`.
pub fn parse_rbn_spots(csv_path: &Path, spotter: &str) -> Result<Vec<OracleSpot>> {
    let text = std::fs::read_to_string(csv_path).with_context(|| format!("read {}", csv_path.display()))?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().context("empty csv")?.split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).with_context(|| format!("missing column {name}"));
    let (c_sp, c_freq, c_dx, c_db, c_date, c_speed) = (col("callsign")?, col("freq")?, col("dx")?, col("db")?, col("date")?, col("speed")?);
    let mut seen = std::collections::BTreeMap::<(String, i64), OracleSpot>::new();
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() <= c_speed || f[c_sp] != spotter { continue; }
        let khz: f64 = f[c_freq].parse()?;
        let hms = &f[c_date][11..19];
        let t_sec = hms[3..5].parse::<f64>()? * 60.0 + hms[6..8].parse::<f64>()?;
        let spot = OracleSpot { call: f[c_dx].to_uppercase(), khz, t_sec, snr_db: f[c_db].parse().unwrap_or(0), wpm: f[c_speed].parse().unwrap_or(0) };
        let key = (spot.call.clone(), (khz * 10.0).round() as i64);
        match seen.get(&key) { Some(s) if s.t_sec <= t_sec => {}, _ => { seen.insert(key, spot); } }
    }
    Ok(seen.into_values().collect())
}

pub fn score_text(call: &str, text: &str) -> (bool, bool, bool) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let as_word = words.iter().any(|w| *w == call);
    let framed = words.windows(2).any(|w| matches!(w[0], "CQ" | "DE" | "TEST") && w[1] == call)
        || words.windows(3).any(|w| w[0] == "CQ" && w[1] == "TEST" && w[2] == call);
    let substring = text.replace(' ', "").contains(call);
    (as_word, framed, substring)
}

pub fn run_oracle(iq: &[Complex32], fs: f64, center_hz: f64, spots: &[OracleSpot], window_s: f64, cfg: &DecodeConfig) -> Result<(Vec<OracleResult>, OracleSummary)> {
    let mut results = Vec::with_capacity(spots.len());
    for spot in spots {
        let mut ch = Channelizer::new(fs, center_hz).map_err(anyhow::Error::msg)?;
        let n = ch.n_channels();
        let delta = fs / n as f64;
        let k0 = ((((spot.khz * 1000.0 - center_hz) / delta).round() as i64).rem_euclid(n as i64)) as usize;
        let t0 = (spot.t_sec - window_s / 2.0).max(0.0);
        let s0 = (t0 * fs) as usize;
        let s1 = ((t0 + window_s) * fs) as usize;
        if s0 >= iq.len() { continue; }
        let hops = ch.process(&iq[s0..s1.min(iq.len())]);
        let cands = [(k0 + n - 1) % n, k0, (k0 + 1) % n];
        let mean = |k: usize| hops.iter().map(|h| h.power[k] as f64).sum::<f64>() / hops.len().max(1) as f64;
        let own = *cands.iter().max_by(|&&a, &&b| mean(a).total_cmp(&mean(b))).unwrap();
        let mut dec = TrackDecoder::new(1, cfg.clone());
        let mut events = Vec::new();
        for (i, h) in hops.iter().enumerate() {
            let p = h.power[own];
            events.extend(dec.push_hop(p.sqrt(), p, None, (s0 + i * ch.hop()) as u64));
        }
        events.extend(dec.finish());
        let text = events_to_text(&events);
        let wpm = events.iter().rev().find_map(|e| match e { DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm), _ => None });
        let (as_word, framed, substring) = score_text(&spot.call, &text);
        let wpm_ratio = wpm.filter(|_| spot.wpm > 0).map(|w| w / spot.wpm as f32);
        results.push(OracleResult { call: spot.call.clone(), khz: spot.khz, as_word, framed, substring, wpm_ratio, text });
    }
    let summary = summarize(spots, &results);
    Ok((results, summary))
}

fn summarize(spots: &[OracleSpot], results: &[OracleResult]) -> OracleSummary {
    let mut ratios: Vec<f32> = results.iter().filter_map(|r| r.wpm_ratio).collect();
    ratios.sort_by(f32::total_cmp);
    let median = if ratios.is_empty() { None } else { Some(ratios[ratios.len() / 2]) };
    let bucket = |snr: i32| if snr < 15 { "lt15" } else if snr < 25 { "15-25" } else { "ge25" };
    let mut by = std::collections::BTreeMap::<&str, (usize, usize, usize)>::new();
    for (s, r) in spots.iter().zip(results) {
        let e = by.entry(bucket(s.snr_db)).or_default();
        e.0 += 1; e.1 += r.as_word as usize; e.2 += r.framed as usize;
    }
    OracleSummary {
        n: results.len(),
        as_word: results.iter().filter(|r| r.as_word).count(),
        framed: results.iter().filter(|r| r.framed).count(),
        substring: results.iter().filter(|r| r.substring).count(),
        wpm_ratio_median: median,
        by_snr: by.into_iter().map(|(k, (n, a, f))| (k.to_string(), n, a, f)).collect(),
    }
}
```

(The `zip` in `summarize` assumes one result per spot; keep the `continue` above for out-of-range spots and, when it fires, push a result with all-false fields so the lengths match.)

CLI (`main.rs`): add to `Command`:

```rust
    /// Real-signal decode oracle: decode each RBN-spotted station's channel
    /// directly (tracker bypassed) and report callsign recovery (SPEC v2 §8.3).
    Oracle {
        /// Stereo IQ WAV with <stem>.json sidecar.
        path: PathBuf,
        /// RBN daily-dump CSV pre-filtered to the recording's window.
        rbn_csv: PathBuf,
        /// Spotter whose spots define the reference set (the co-located skimmer).
        #[arg(long, default_value = "K5TR")]
        spotter: String,
        #[arg(long, default_value_t = 40.0)]
        window_s: f64,
        #[arg(long, default_value = "legacy")]
        engine: manta_decode::decoder::Engine,
        /// Write per-spot results as JSON Lines here (summary always goes to stdout).
        #[arg(long)]
        jsonl: Option<PathBuf>,
    },
```

and `--engine` (same type, default `legacy`) on `Decode`, stored into `cfg.decode.engine` in `build_pipeline_config` (add a parameter). Dispatch:

```rust
        Command::Oracle { path, rbn_csv, spotter, window_s, engine, jsonl } => {
            let mut src = manta_input::WavIqSource::open(&path)?;
            let (fs, center) = (src.sample_rate(), src.center_freq_hz());
            let iq = manta_input::read_all(&mut src)?;
            let spots = manta_testkit::oracle::parse_rbn_spots(&rbn_csv, &spotter)?;
            let mut cfg = manta_decode::decoder::DecodeConfig::default();
            cfg.engine = engine;
            let (results, summary) = manta_testkit::oracle::run_oracle(&iq, fs, center, &spots, window_s, &cfg)?;
            if let Some(p) = jsonl {
                let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
                for r in &results { use std::io::Write; writeln!(w, "{}", serde_json::to_string(r)?)?; }
            }
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
```

`Engine` needs `clap::ValueEnum` or the `FromStr` from Task 5 (clap uses `FromStr` by default for `value_parser`; the `FromStr` is enough). `manta-cli` must depend on `manta-testkit` (check `Cargo.toml`; `gen` already uses it).

- [ ] **Step 4: Run tests**

Run: `cargo test -p manta-testkit --test oracle_synthetic && cargo test -p manta-cli`
Expected: pass.

- [ ] **Step 5: Measure the stage-1 gate on real signal (local-only files)**

Run (files are local, not in git):

```bash
cargo run --release -p manta-cli -- oracle crates/manta-testkit/audio-corpus/B2_20251129_000000_7080kHz.wav crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv --engine legacy
cargo run --release -p manta-cli -- oracle crates/manta-testkit/audio-corpus/B2_20251129_000000_7080kHz.wav crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv --engine edge-legacy
```

Expected: legacy ≈ `as_word 27 %, framed 13 %, substring 48 %, wpm_ratio ≈ 0.82` (the 2026-09-09 baseline; the exact numbers depend on window placement — record whatever this harness prints as the new baseline). Stage-1 gate: edge-legacy `as_word ≥ 45 %`, `wpm_ratio ∈ [0.95, 1.05]`. Write both summaries into `docs/DECISIONS/2026-09-XX-decode-core-v2-stage1-gate.md` with the commit SHA. If the gate fails, the doc records the numbers and the most common failure texts (from `--jsonl`), and the next step is Task 8, not tuning.

- [ ] **Step 6: Restore `CHAR_GAP_DITS` to 2.0 for `EdgeLegacy`**

In `timing.rs`, `GapClassifier::classify` reads `CHAR_GAP_DITS` (1.6). Make the element/character boundary a field (`char_gap_dits: f32`) set by `GapClassifier::new_with(char_gap_dits)`; `TrackDecoder::new` passes `1.6` for `Engine::Legacy` and `2.0` for `EdgeLegacy`/`Hsmm`. Re-run the workspace tests and the oracle; V2 (`golden_v2_v3.rs`) with `--engine edge-legacy` should now pass — add that as `v2_passes_with_edge_legacy_engine` in `golden_v2_v3.rs` (copy the existing V2 test body, add `"--engine", "edge-legacy"` to the args).

- [ ] **Step 7: Commit**

```bash
git add crates/manta-testkit crates/manta-cli crates/manta-decode docs/DECISIONS
git commit -m "feat(manta-testkit): real-signal decode oracle + manta oracle/--engine (SPEC v2 §8.3), stage-1 gate measured"
```

---

### Task 7: HSMM segments and tokens

**Files:**
- Create: `crates/manta-decode/src/hsmm/segments.rs`, `crates/manta-decode/src/hsmm/token.rs`, `crates/manta-decode/src/hsmm/mod.rs` (module skeleton + `HsmmConfig`)
- Modify: `crates/manta-decode/src/lib.rs` (`pub mod hsmm;`)

**Interfaces:**
- Produces:

```rust
pub struct HsmmConfig { pub dur_sigma: f32, pub mark_insert_penalty: f32, pub beam: usize, pub lookahead_dits: f32, pub speed_alpha: f32, pub seed_units_hops: Vec<f32>, pub conf_kappa: f32, pub u_min: f32, pub u_max: f32 }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Phase { AfterSpace, AfterMark }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum SegType { Dit, Dah, EGap, CGap, WGap, Silence }
impl SegType { pub const ALL: [SegType; 6]; pub fn is_mark(self) -> bool; pub fn k(self) -> f32; pub fn updates_speed(self) -> bool; pub fn log_type_prior(self, cfg: &HsmmConfig) -> f32; pub fn log_dur_prior(self, d: u32, u: f32, cfg: &HsmmConfig) -> Option<f32>; pub fn ends_phase(self) -> Phase /* the phase a token must be in to take it */ }
#[derive(Clone, Copy, Debug, PartialEq)] pub struct HistEntry { pub glyph: Option<Glyph> /* None = word boundary */, pub sample_ts: u64, pub born_hop: u64 }
#[derive(Clone, Debug)] pub struct Token { pub node: NodeId, pub phase: Phase, pub u: f32, pub score: f32, pub hist: Vec<HistEntry>, pub anchor_hop: u64 }
pub fn glyph_rank(g: Option<Glyph>) -> u32;   // Word = 0, Prosign = 1..=5, Char(c) = 16 + c as u32
pub fn token_order(a: &Token, b: &Token) -> Ordering;   // v2 §6.2
pub fn merge_key(t: &Token) -> (NodeId, Phase, i32);     // (node, phase, round(2u))
impl Token { pub fn successor(&self, seg: SegType, d: u32, evidence: f32, tree: &MorseTree, cfg: &HsmmConfig, end_hop: u64, seg_start_ts: u64) -> Option<Token>; }
```

- [ ] **Step 1: Failing tests**

```rust
// hsmm/segments.rs tests
    #[test]
    fn duration_prior_peaks_at_nominal_and_is_bounded() {
        let cfg = HsmmConfig::default();
        assert_eq!(SegType::Dah.log_dur_prior(39, 13.0, &cfg), Some(0.0));
        assert!(SegType::Dah.log_dur_prior(30, 13.0, &cfg).unwrap() < -1.0);
        assert_eq!(SegType::Dah.log_dur_prior(20, 13.0, &cfg), None);    // < 0.6 * 39
        assert_eq!(SegType::Dah.log_dur_prior(60, 13.0, &cfg), None);    // > 1.5 * 39
        assert_eq!(SegType::Silence.log_dur_prior(200, 13.0, &cfg), Some(0.0));
        assert_eq!(SegType::Silence.log_dur_prior(100, 13.0, &cfg), None);  // < 10u
    }

    #[test]
    fn mark_type_prior_includes_insertion_penalty() {
        let cfg = HsmmConfig::default();
        assert!((SegType::Dit.log_type_prior(&cfg) - (0.6f32.ln() - 1.5)).abs() < 1e-6);
        assert!((SegType::WGap.log_type_prior(&cfg) - 0.10f32.ln()).abs() < 1e-6);
    }
```

```rust
// hsmm/token.rs tests
    use crate::tree::{Element, MorseTree};

    fn root_token(u: f32) -> Token { Token { node: MorseTree::ROOT, phase: Phase::AfterSpace, u, score: 0.0, hist: Vec::new(), anchor_hop: 0 } }

    #[test]
    fn dit_then_dah_then_cgap_emits_a() {
        let tree = MorseTree::shared(); let cfg = HsmmConfig::default();
        let t = root_token(13.0);
        let t = t.successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0).unwrap();
        assert_eq!(t.phase, Phase::AfterMark);
        assert_eq!(t.node, tree.child(MorseTree::ROOT, Element::Dit).unwrap());
        let t = t.successor(SegType::EGap, 13, 50.0, tree, &cfg, 26, 13 * 512).unwrap();
        let t = t.successor(SegType::Dah, 39, 150.0, tree, &cfg, 65, 26 * 512).unwrap();
        let t = t.successor(SegType::CGap, 39, 150.0, tree, &cfg, 104, 65 * 512).unwrap();
        assert_eq!(t.node, MorseTree::ROOT);
        assert_eq!(t.hist.len(), 1);
        assert_eq!(t.hist[0].glyph, Some(Glyph::Char('A')));
        assert_eq!(t.hist[0].sample_ts, 65 * 512);
        assert!((t.u - 13.0).abs() < 1e-4, "u drifted to {}", t.u);
    }

    #[test]
    fn invalid_transitions_return_none() {
        let tree = MorseTree::shared(); let cfg = HsmmConfig::default();
        let t = root_token(13.0);
        assert!(t.successor(SegType::EGap, 13, 0.0, tree, &cfg, 13, 0).is_none());        // space from AfterSpace
        assert!(t.successor(SegType::CGap, 39, 0.0, tree, &cfg, 39, 0).is_none());
        let t = t.successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0).unwrap();
        assert!(t.successor(SegType::Dit, 13, 50.0, tree, &cfg, 26, 0).is_none());        // mark from AfterMark
        // 8 dits deep: fall off the tree
        let mut t = root_token(13.0);
        for i in 0..8 {
            let m = t.successor(SegType::Dit, 13, 50.0, tree, &cfg, 26 * i + 13, 0);
            match m { Some(m) => t = m.successor(SegType::EGap, 13, 50.0, tree, &cfg, 26 * i + 26, 0).unwrap(),
                      None => { assert!(i >= 5, "fell off at depth {i}"); return; } }
        }
        panic!("never fell off the tree");
    }

    #[test]
    fn speed_updates_only_on_short_segments() {
        let tree = MorseTree::shared(); let cfg = HsmmConfig::default();
        let t = root_token(10.0).successor(SegType::Dit, 13, 50.0, tree, &cfg, 13, 0).unwrap();
        assert!((t.u - (10.0 + 0.2 * (13.0 - 10.0))).abs() < 1e-5);
        let t = t.successor(SegType::WGap, 91, 300.0, tree, &cfg, 104, 0);
        assert!(t.is_none(), "WGap from a glyphless node must be invalid");
        let e = root_token(13.0).successor(SegType::Dah, 39, 100.0, tree, &cfg, 39, 0).unwrap(); // node T
        let w = e.successor(SegType::WGap, 100, 100.0, tree, &cfg, 139, 0).unwrap();
        assert!((w.u - e.u).abs() < 1e-6, "WGap must not update u");
        assert_eq!(w.hist.len(), 2);   // glyph T, then Word
        assert_eq!(w.hist[1].glyph, None);
    }

    #[test]
    fn ordering_is_total_and_matches_spec() {
        let mut a = root_token(13.0); a.score = 1.0;
        let mut b = root_token(13.0); b.score = 2.0;
        assert_eq!(token_order(&b, &a), std::cmp::Ordering::Less);   // higher score first
        let mut c = root_token(12.0); c.score = 1.0;
        assert_eq!(token_order(&c, &a), std::cmp::Ordering::Less);   // equal score: smaller u first
        assert_eq!(merge_key(&a), (MorseTree::ROOT, Phase::AfterSpace, 26));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-decode hsmm::`
Expected: compile error.

- [ ] **Step 3: Implement `segments.rs`**

```rust
//! Segment types, duration and type priors. SPEC v2 §4.2–4.4.
use super::HsmmConfig;
use super::token::Phase;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegType { Dit, Dah, EGap, CGap, WGap, Silence }

impl SegType {
    pub const ALL: [SegType; 6] = [SegType::Dit, SegType::Dah, SegType::EGap, SegType::CGap, SegType::WGap, SegType::Silence];

    pub fn is_mark(self) -> bool { matches!(self, SegType::Dit | SegType::Dah) }

    /// Nominal length in dits.
    pub fn k(self) -> f32 {
        match self { SegType::Dit | SegType::EGap => 1.0, SegType::Dah | SegType::CGap => 3.0, SegType::WGap => 7.0, SegType::Silence => 10.0 }
    }

    pub fn updates_speed(self) -> bool { !matches!(self, SegType::WGap | SegType::Silence) }

    /// The phase a token must be in to end this segment.
    pub fn from_phase(self) -> Phase { if self.is_mark() { Phase::AfterSpace } else { Phase::AfterMark } }

    pub fn log_type_prior(self, cfg: &HsmmConfig) -> f32 {
        match self {
            SegType::Dit => 0.6f32.ln() + cfg.mark_insert_penalty,
            SegType::Dah => 0.4f32.ln() + cfg.mark_insert_penalty,
            SegType::EGap => 0.62f32.ln(),
            SegType::CGap => 0.28f32.ln(),
            SegType::WGap => 0.10f32.ln(),
            SegType::Silence => 0.10f32.ln() - 4.0,
        }
    }

    /// Log-normal duration prior about k*u; None outside [0.6, 1.5]*nominal
    /// (Silence: flat on [10u, 80u]). SPEC v2 §4.3.
    pub fn log_dur_prior(self, d: u32, u: f32, cfg: &HsmmConfig) -> Option<f32> {
        let d = d as f32;
        if self == SegType::Silence {
            return if d >= 10.0 * u && d <= 80.0 * u { Some(0.0) } else { None };
        }
        let nom = self.k() * u;
        if d < 0.6 * nom || d > 1.5 * nom { return None; }
        let x = (d / nom).ln();
        Some(-(x * x) / (2.0 * cfg.dur_sigma * cfg.dur_sigma))
    }
}
```

- [ ] **Step 4: Implement `token.rs`**

```rust
//! Tokens: hypotheses over (tree node, phase, speed) with committed-glyph
//! history. SPEC v2 §4.5, §6.2.
use super::segments::SegType;
use super::HsmmConfig;
use crate::tree::{Element, Glyph, MorseTree, NodeId, Prosign};
use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Phase { AfterSpace, AfterMark }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistEntry { pub glyph: Option<Glyph>, pub sample_ts: u64, pub born_hop: u64 }

#[derive(Clone, Debug)]
pub struct Token { pub node: NodeId, pub phase: Phase, pub u: f32, pub score: f32, pub hist: Vec<HistEntry>, pub anchor_hop: u64 }

pub fn glyph_rank(g: Option<Glyph>) -> u32 {
    match g {
        None => 0,
        Some(Glyph::Prosign(p)) => match p { Prosign::Ar => 1, Prosign::Sk => 2, Prosign::As => 3, Prosign::Sn => 4, Prosign::Err => 5 },
        Some(Glyph::Char(c)) => 16 + c as u32,
    }
}

/// SPEC v2 §6.2: score desc, hist lexical, u asc, node asc, phase.
pub fn token_order(a: &Token, b: &Token) -> Ordering {
    b.score.total_cmp(&a.score)
        .then_with(|| a.hist.iter().map(|e| glyph_rank(e.glyph)).cmp(b.hist.iter().map(|e| glyph_rank(e.glyph))))
        .then_with(|| a.u.total_cmp(&b.u))
        .then_with(|| a.node.cmp(&b.node))
        .then_with(|| a.phase.cmp(&b.phase))
}

pub fn merge_key(t: &Token) -> (NodeId, Phase, i32) { (t.node, t.phase, (2.0 * t.u).round() as i32) }

impl Token {
    /// The token reached by ending segment `seg` of `d` hops with mark
    /// evidence `evidence` (= C[t] − C[s]; negated internally for spaces).
    /// `seg_start_ts` is the input sample counter at the segment's first hop
    /// (the timestamp a glyph committed by this gap carries, pinned decision 11).
    pub fn successor(&self, seg: SegType, d: u32, evidence: f32, tree: &MorseTree, cfg: &HsmmConfig, end_hop: u64, seg_start_ts: u64) -> Option<Token> {
        if self.phase != seg.from_phase() { return None; }
        let lp_dur = seg.log_dur_prior(d, self.u, cfg)?;
        let ev = if seg.is_mark() { evidence } else { -evidence };
        let score = self.score + ev + lp_dur + seg.log_type_prior(cfg);
        let mut u = self.u;
        if seg.updates_speed() {
            u += cfg.speed_alpha * (d as f32 / seg.k() - u);
            u = u.clamp(cfg.u_min, cfg.u_max);
        }
        let mut hist = self.hist.clone();
        let (node, phase) = match seg {
            SegType::Dit | SegType::Dah => {
                let e = if seg == SegType::Dit { Element::Dit } else { Element::Dah };
                (tree.child(self.node, e)?, Phase::AfterMark)
            }
            SegType::EGap => (self.node, Phase::AfterSpace),
            SegType::CGap | SegType::WGap | SegType::Silence => {
                let g = tree.glyph(self.node)?;
                hist.push(HistEntry { glyph: Some(g), sample_ts: seg_start_ts, born_hop: end_hop });
                if seg != SegType::CGap { hist.push(HistEntry { glyph: None, sample_ts: seg_start_ts, born_hop: end_hop }); }
                (MorseTree::ROOT, Phase::AfterSpace)
            }
        };
        Some(Token { node, phase, u, score, hist, anchor_hop: end_hop })
    }
}
```

`mod.rs` skeleton for this task:

```rust
//! Hidden semi-Markov token-passing Morse decoder. SPEC v2 §4.
pub mod segments;
pub mod token;

#[derive(Debug, Clone)]
pub struct HsmmConfig { pub dur_sigma: f32, pub mark_insert_penalty: f32, pub beam: usize, pub lookahead_dits: f32, pub speed_alpha: f32, pub seed_units_hops: Vec<f32>, pub conf_kappa: f32, pub u_min: f32, pub u_max: f32 }

impl Default for HsmmConfig {
    fn default() -> Self {
        HsmmConfig { dur_sigma: 0.22, mark_insert_penalty: -1.5, beam: 12, lookahead_dits: 25.0, speed_alpha: 0.2, seed_units_hops: vec![9.0, 13.0, 18.0, 26.0, 38.0], conf_kappa: 6.0, u_min: 7.5, u_max: 56.0 }
    }
}
```

- [ ] **Step 5: Run tests and commit**

Run: `cargo test -p manta-decode hsmm::` → all pass.

```bash
git add crates/manta-decode/src/hsmm crates/manta-decode/src/lib.rs
git commit -m "feat(manta-decode): HSMM segment priors and tokens (SPEC v2 §4.2-4.5)"
```

---

### Task 8: HSMM decoder loop, commit, confidence, engine wiring

**Files:**
- Modify: `crates/manta-decode/src/hsmm/mod.rs` (add `HsmmDecoder`)
- Create: `crates/manta-decode/src/hsmm/commit.rs`
- Modify: `crates/manta-decode/src/decoder.rs` (`push_hop_hsmm`, `finish`)

**Interfaces:**
- Produces:

```rust
pub struct HsmmDecoder { … }
impl HsmmDecoder {
    pub fn new(cfg: HsmmConfig) -> Self;
    /// Feed one evidence hop; returns committed entries (glyph or word) with confidence and alternatives.
    pub fn push(&mut self, ev: &HopEvidence) -> Vec<Committed>;
    pub fn finish(&mut self) -> Vec<Committed>;
    pub fn best_u(&self) -> Option<f32>;
}
pub struct Committed { pub glyph: Option<Glyph>, pub sample_ts: u64, pub confidence: f32, pub alternatives: Vec<(Glyph, f32)> }
```

- [ ] **Step 1: Failing tests (in `decoder.rs`, reusing Task 5's helpers)**

```rust
    #[test]
    fn hsmm_decodes_clean_text_at_three_speeds() {
        for (dit_hops, label) in [(24u32, "19 WPM"), (13, "35 WPM"), (10, "45 WPM")] {
            let env = rect_envelope_depth("CQ TEST W5AU W5AU TEST", dit_hops, 40.0);
            assert_eq!(decode_with(Engine::Hsmm, &env), "CQ TEST W5AU W5AU TEST", "{label}");
        }
    }

    #[test]
    fn hsmm_speed_converges_from_seeds() {
        let env = rect_envelope_depth("PARIS PARIS PARIS", 13, 40.0);
        let mut cfg = DecodeConfig::default(); cfg.engine = Engine::Hsmm;
        let mut dec = TrackDecoder::new(1, cfg);
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() { events.extend(dec.push_envelope(a, i as u64 * 256)); }
        events.extend(dec.finish());
        let wpm = events.iter().rev().find_map(|e| match e { DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm), _ => None }).unwrap();
        assert!((wpm - 34.6).abs() < 2.0, "wpm {wpm}");
    }

    #[test]
    fn hsmm_commits_within_lookahead() {
        // After "E" and a long silence, the E must be committed before 25 dits of silence pass.
        let mut env = rect_envelope_depth("E", 13, 40.0);
        env.truncate(13 + 4);                      // the dit plus 4 hops of space
        env.extend(std::iter::repeat(0.01).take(13 * 40));
        let mut cfg = DecodeConfig::default(); cfg.engine = Engine::Hsmm;
        let mut dec = TrackDecoder::new(1, cfg);
        let mut first_char_hop = None;
        for (i, &a) in env.iter().enumerate() {
            for e in dec.push_envelope(a, i as u64 * 256) {
                if matches!(e, DecoderEvent::CharDecoded { .. }) && first_char_hop.is_none() { first_char_hop = Some(i); }
            }
        }
        let h = first_char_hop.expect("E was never committed");
        assert!(h < 13 + 13 * 25 + 60, "committed at hop {h}");
    }

    #[test]
    fn hsmm_is_bit_deterministic() {
        let env = rect_envelope_depth("CQ TEST W5AU", 13, 40.0);
        let run = || {
            let mut cfg = DecodeConfig::default(); cfg.engine = Engine::Hsmm;
            let mut dec = TrackDecoder::new(1, cfg);
            let mut ev = Vec::new();
            for (i, &a) in env.iter().enumerate() { ev.extend(dec.push_envelope(a, i as u64 * 256)); }
            ev.extend(dec.finish());
            serde_json::to_string(&ev).unwrap()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn hsmm_confidence_and_alternatives_are_populated() {
        let env = rect_envelope_depth("CQ TEST W5AU", 13, 40.0);
        let mut cfg = DecodeConfig::default(); cfg.engine = Engine::Hsmm;
        let mut dec = TrackDecoder::new(1, cfg);
        let mut ev = Vec::new();
        for (i, &a) in env.iter().enumerate() { ev.extend(dec.push_envelope(a, i as u64 * 256)); }
        ev.extend(dec.finish());
        let confs: Vec<f32> = ev.iter().filter_map(|e| match e { DecoderEvent::CharDecoded { confidence, .. } => Some(*confidence), _ => None }).collect();
        assert!(!confs.is_empty() && confs.iter().all(|c| *c > 0.0 && *c <= 1.0));
        assert!(confs.iter().sum::<f32>() / confs.len() as f32 > 0.6, "clean text should be confident: {confs:?}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p manta-decode hsmm_`
Expected: panics at `unimplemented!("Task 8")`.

- [ ] **Step 3: Implement `HsmmDecoder` (`hsmm/mod.rs`)**

```rust
use crate::evidence::HopEvidence;
use crate::tree::{Glyph, MorseTree};
use segments::SegType;
use std::collections::VecDeque;
use token::{merge_key, token_order, HistEntry, Phase, Token};

#[derive(Debug, Clone)]
pub struct Committed { pub glyph: Option<Glyph>, pub sample_ts: u64, pub confidence: f32, pub alternatives: Vec<(Glyph, f32)> }

struct Anchor { hop: u64, sample_ts: u64, prefix: f64, tokens: Vec<Token> }

pub struct HsmmDecoder {
    cfg: HsmmConfig,
    anchors: VecDeque<Anchor>,
    present: bool,
    /// Tokens alive at the most recent anchor (also stored in anchors.back()).
    live: Vec<Token>,
    hop: u64,
    last_prefix: f64,
    last_ts: u64,
}

impl HsmmDecoder {
    pub fn new(cfg: HsmmConfig) -> Self {
        HsmmDecoder { cfg, anchors: VecDeque::new(), present: false, live: Vec::new(), hop: 0, last_prefix: 0.0, last_ts: 0 }
    }

    pub fn best_u(&self) -> Option<f32> { self.live.first().map(|t| t.u) }

    fn seed(&mut self, hop: u64) -> Vec<Token> {
        self.cfg.seed_units_hops.iter().map(|&u| Token { node: MorseTree::ROOT, phase: Phase::AfterSpace, u, score: 0.0, hist: Vec::new(), anchor_hop: hop }).collect()
    }

    pub fn push(&mut self, ev: &HopEvidence) -> Vec<Committed> {
        self.hop = ev.hop; self.last_prefix = ev.prefix; self.last_ts = ev.sample_ts;
        let mut out = Vec::new();
        if ev.present && !self.present {
            // Keying just appeared: (re)seed at this hop.
            let seeds = self.seed(ev.hop);
            self.anchors.push_back(Anchor { hop: ev.hop, sample_ts: ev.sample_ts, prefix: ev.prefix, tokens: seeds.clone() });
            self.live = seeds;
        }
        self.present = ev.present;
        if !ev.anchor || self.anchors.is_empty() { return out; }
        let tree = MorseTree::shared();
        let u_max = self.live.iter().map(|t| t.u).fold(self.cfg.u_min, f32::max);
        let reach_norm = (1.5 * 7.0 * u_max).ceil() as u64;
        let reach_sil = (80.0 * u_max).ceil() as u64;
        let mut cands: Vec<Token> = Vec::new();
        for a in self.anchors.iter() {
            let d = ev.hop - a.hop;
            if d == 0 || d > reach_sil { continue; }
            let evidence = (ev.prefix - a.prefix) as f32;
            for tok in &a.tokens {
                for seg in SegType::ALL {
                    if seg == SegType::Silence { if d < (10.0 * tok.u) as u64 { continue; } }
                    else if d > reach_norm { continue; }
                    if let Some(s) = tok.successor(seg, d as u32, evidence, tree, &self.cfg, ev.hop, a.sample_ts) { cands.push(s); }
                }
            }
        }
        if cands.is_empty() { return out; }
        // Merge by (node, phase, round(2u)): keep the best by token_order.
        cands.sort_by(token_order);
        let mut merged: Vec<Token> = Vec::with_capacity(cands.len());
        'outer: for c in cands {
            for m in &merged { if merge_key(m) == merge_key(&c) { continue 'outer; } }
            merged.push(c);
        }
        merged.truncate(self.cfg.beam);
        // Commit consensus / forced entries.
        out.extend(commit::commit(&mut merged, ev.hop, &self.cfg));
        // Re-seed alongside survivors after a Silence (tokens at ROOT with empty hist are seeds).
        if merged.iter().any(|t| t.node == MorseTree::ROOT && t.phase == Phase::AfterSpace && t.anchor_hop == ev.hop && t.hist.is_empty()) {
            // nothing: seeds are only added when keying reappears (present rising edge).
        }
        self.live = merged.clone();
        self.anchors.push_back(Anchor { hop: ev.hop, sample_ts: ev.sample_ts, prefix: ev.prefix, tokens: merged });
        while let Some(front) = self.anchors.front() {
            if ev.hop - front.hop > reach_sil { self.anchors.pop_front(); } else { break; }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<Committed> {
        let mut out = Vec::new();
        if self.live.is_empty() { return out; }
        self.live.sort_by(token_order);
        let best = self.live[0].clone();
        let hist = best.hist.clone();
        for (j, e) in hist.iter().enumerate() {
            let (confidence, alternatives) = commit::margin(&self.live, j, &self.cfg);
            out.push(Committed { glyph: e.glyph, sample_ts: e.sample_ts, confidence, alternatives });
        }
        if best.phase == Phase::AfterMark {
            if let Some(g) = MorseTree::shared().glyph(best.node) {
                out.push(Committed { glyph: Some(g), sample_ts: self.last_ts, confidence: 0.5, alternatives: Vec::new() });
            }
        }
        self.live.clear();
        out
    }
}
```

(The empty `if … { }` block above is a placeholder for nothing — delete it; seeding happens only on the `present` rising edge. The `Silence` segment already returns the token to `ROOT`, so a long pause followed by keying continues from the same tokens with their tracked speed, and a fresh `present` rising edge adds new seeds.)

- [ ] **Step 4: Implement `commit.rs`**

```rust
//! Partial-traceback commit and posterior margins. SPEC v2 §4.8–4.9.
use super::token::{glyph_rank, Token};
use super::{Committed, HsmmConfig};
use crate::tree::Glyph;

fn sigmoid(x: f32) -> f32 { 1.0 / (1.0 + (-x).exp()) }

/// Confidence and alternatives for entry `j` of the best token (tokens
/// must be sorted by `token_order`, best first).
pub fn margin(tokens: &[Token], j: usize, cfg: &HsmmConfig) -> (f32, Vec<(Glyph, f32)>) {
    let best = &tokens[0];
    let g = best.hist.get(j).map(|e| e.glyph);
    let mut alts: Vec<(Option<Glyph>, f32)> = Vec::new();
    for t in tokens.iter().skip(1) {
        let tg = t.hist.get(j).map(|e| e.glyph);
        if tg != g {
            if let Some(og) = tg.flatten() {
                if !alts.iter().any(|(a, _)| *a == Some(og)) { alts.push((Some(og), t.score)); }
            } else if tg.is_some() && !alts.iter().any(|(a, _)| a.is_none()) {
                alts.push((None, t.score));
            }
        }
    }
    let s_alt = alts.iter().map(|(_, s)| *s).fold(f32::NEG_INFINITY, f32::max);
    let s_alt = if s_alt.is_finite() { s_alt } else { best.score - 4.0 * cfg.conf_kappa };
    let confidence = sigmoid((best.score - s_alt) / cfg.conf_kappa);
    alts.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| glyph_rank(a.0).cmp(&glyph_rank(b.0))));
    let alternatives = alts.into_iter().filter_map(|(g, s)| g.map(|g| (g, sigmoid((s - best.score) / cfg.conf_kappa)))).take(2).collect();
    (confidence, alternatives)
}

/// Emit the consensus prefix and any entry older than the lookahead;
/// remove emitted entries from every token; drop tokens that disagree
/// with a forced commit. `tokens` is sorted best-first on entry and
/// stays sorted.
pub fn commit(tokens: &mut Vec<Token>, now_hop: u64, cfg: &HsmmConfig) -> Vec<Committed> {
    let mut out = Vec::new();
    loop {
        if tokens.is_empty() || tokens[0].hist.is_empty() { break; }
        let head = tokens[0].hist[0];
        let consensus = tokens.iter().all(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        let forced = (now_hop - head.born_hop) as f32 > cfg.lookahead_dits * tokens[0].u;
        if !consensus && !forced { break; }
        let (confidence, alternatives) = margin(tokens, 0, cfg);
        out.push(Committed { glyph: head.glyph, sample_ts: head.sample_ts, confidence, alternatives });
        tokens.retain(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        for t in tokens.iter_mut() { t.hist.remove(0); }
    }
    out
}
```

- [ ] **Step 5: Wire into `TrackDecoder`**

In `decoder.rs`: field `hsmm: HsmmDecoder` (constructed from `cfg.hsmm: HsmmConfig`, add that field to `DecodeConfig` with `Default`), and:

```rust
    fn push_hop_hsmm(&mut self, amp: f32, power: f32, spectral_ref_power: Option<f32>, sample_ts: u64) -> Vec<DecoderEvent> {
        let n = self.noise.push(power, spectral_ref_power).max(1e-18).sqrt();
        let mut events = Vec::new();
        if let Some(ev) = self.evidence.push(amp, n, sample_ts) {
            self.snr_from_evidence(&ev);
            for c in self.hsmm.push(&ev) { self.emit_committed(c, &mut events); }
            if let Some(u) = self.hsmm.best_u() { self.evidence.set_u_ref(u); self.report_wpm(450.0 / u, &mut events); }
        }
        self.tick_meta(&mut events);
        events
    }

    fn emit_committed(&mut self, c: Committed, events: &mut Vec<DecoderEvent>) {
        let q = self.last_snr.map(|s| (s / 20.0).clamp(0.3, 1.0)).unwrap_or(1.0);
        match c.glyph {
            Some(glyph) => events.push(DecoderEvent::CharDecoded { track_id: self.track_id, sample_ts: c.sample_ts, glyph, confidence: c.confidence * q, alternatives: c.alternatives }),
            None => events.push(DecoderEvent::WordBoundary { track_id: self.track_id, sample_ts: c.sample_ts, confidence: c.confidence }),
        }
    }
```

`report_wpm` is the existing ≥ 1 WPM change rule factored out of `process_run`. `finish()` for `Engine::Hsmm`: drain `self.evidence.flush()` through `self.hsmm.push`, then `self.hsmm.finish()` through `emit_committed`.

- [ ] **Step 6: Run tests; debug until green**

Run: `cargo test -p manta-decode`
Expected: all pass. Likely first failures and where to look: (a) `hsmm_decodes_clean_text_at_three_speeds` at 45 WPM — check `u_min` (7.5) and that the 4-hop ramp is not producing anchors that split a 10-hop dit (`fallback_hops = 8` also fires inside dits; that is fine, the extra anchor is just an extra candidate start); (b) consensus never reached because seeds at very different speeds keep diverging paths alive — the merge key includes `round(2u)`, so slow seeds die by score within one character; if not, the insertion penalty is too weak; (c) `hsmm_commits_within_lookahead` — `born_hop` must be the hop the glyph entry was *created* (the closing gap's end), which is what `successor` stores.

- [ ] **Step 7: Commit**

```bash
git add crates/manta-decode
git commit -m "feat(manta-decode): HSMM token-passing decoder engine with partial-traceback commit and posteriors (SPEC v2 §4.6-4.10)"
```

---

### Task 9: Configuration plumbing

**Files:**
- Modify: `crates/manta-cli/src/main.rs` (`--engine` on `decode`/`listen`/`run`, and the `[decode]` TOML table where it is parsed — `grep -n "decode" crates/manta-cli/src/config.rs crates/manta-server/src/config.rs`)
- Modify: `docs/SPEC-decode-core.md` §9 (pointer to v2 §7)

**Interfaces:**
- Consumes: `Engine`, `EvidenceConfig`, `NoiseConfig`, `HsmmConfig` (Tasks 3–8).

- [ ] **Step 1: Failing test**

In `crates/manta-cli/tests/cli.rs` add:

```rust
#[test]
fn decode_accepts_engine_flag() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    for engine in ["legacy", "edge-legacy", "hsmm"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["decode", "--engine", engine]).arg(dir.path().join("v1.wav")).output().unwrap();
        assert!(out.status.success(), "{engine}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains("W1AW"), "{engine} lost W1AW");
    }
}
```

- [ ] **Step 2: Run to verify failure**, then **Step 3: implement**: every v2 §7 key under `[decode]` in the TOML config struct (serde `#[serde(default)]` on each, defaults from the `Default` impls), threaded into `PipelineConfig.decode`. The CLI flag overrides the file.

- [ ] **Step 4: Run `cargo test --workspace`; commit**

```bash
git commit -am "feat(manta-cli): decode.engine and SPEC v2 §7 config keys"
```

---

### Task 10: Real-conditions golden vectors VR1–VR8

**Files:**
- Modify: `crates/manta-testkit/src/keyer.rs` (`weight: f32` dah/dit ratio, `char_gap_units: f32`, `word_gap_units: f32` on `KeyerSpec`, all defaulting to 3.0/3.0/7.0), `scene.rs` (`SignalSpec` gains the same three fields; a second `SignalSpec` at the same offset is already a co-channel pair — VR5 needs nothing new), `vectors.rs` (`vr1()`…`vr8()`)
- Modify: `crates/manta-cli/src/main.rs` (`gen vr1`…`vr8`)
- Create: `crates/manta-cli/tests/golden_vr.rs`

- [ ] **Step 1: Failing keyer test**

```rust
// keyer.rs tests
    #[test]
    fn weight_and_gap_units_change_segment_lengths() {
        let mut spec = KeyerSpec::new(30.0);
        spec.weight = 2.6; spec.char_gap_units = 2.5; spec.word_gap_units = 5.0;
        let (env, _) = key_text("E T", &spec, 3000.0).unwrap();       // 3 kHz render for cheap counting
        // dit = 40 ms = 120 samples; dah = 2.6 dits = 312; word gap = 5 dits = 600
        let on: Vec<bool> = env.iter().map(|&v| v > 0.5).collect();
        let mut runs = vec![]; let mut cur = on[0]; let mut n = 0;
        for &v in &on { if v == cur { n += 1 } else { runs.push((cur, n)); cur = v; n = 1; } } runs.push((cur, n));
        assert!((runs[0].1 as i32 - 120).abs() <= 2, "dit {runs:?}");
        assert!((runs[1].1 as i32 - 600).abs() <= 2, "word gap {runs:?}");
        assert!((runs[2].1 as i32 - 312).abs() <= 2, "dah {runs:?}");
    }
```

- [ ] **Step 2: Implement**: in `push_word`, dah = `weight * unit`, character gap = `char_gap_units * gap_unit`; in `key_text`/`key_text_loop`, word gap = `word_gap_units * gap_unit`. Existing callers keep 3.0/3.0/7.0 defaults (`KeyerSpec::new` sets them; `render_scene` copies from `SignalSpec`, whose new fields default via a `SignalSpec::default_timing()` helper used by every existing `vN()` constructor — add the three fields to every existing `SignalSpec { … }` literal with `weight: 3.0, char_gap_units: 3.0, word_gap_units: 7.0`, or derive them through a `..SignalSpec::base()` if you add a `base()` constructor).

- [ ] **Step 3: Vectors** — implement exactly the v2 §8.2 table:

```rust
pub fn vr1() -> VectorSpec { single("vr1", 40.0, 30.0, "CQ TEST W5AU W5AU TEST", 0x5652_0001, Some(Jitter { sigma: 0.08, seed: 1 })) }
pub fn vr2() -> VectorSpec { single("vr2", 45.0, 30.0, "CQ TEST K5TR K5TR TEST", 0x5652_0002, Some(Jitter { sigma: 0.08, seed: 2 })) }
pub fn vr3() -> VectorSpec { single("vr3", 30.0, 45.0, "CQ TEST N5ZZ N5ZZ TEST", 0x5652_0003, None) }
pub fn vr4() -> VectorSpec { let mut v = single("vr4", 32.0, 25.0, "CQ TEST G3ABC G3ABC TEST", 0x5652_0004, None); v.rise_ms = 0.5; v }
pub fn vr5() -> VectorSpec { /* two SignalSpecs: 30 WPM +25 dB at +10_000 Hz, 24 WPM +15 dB at +10_030 Hz, texts CQ TEST W1AA W1AA TEST / CQ TEST W2BB W2BB TEST */ }
pub fn vr6a() / vr6b() -> VectorSpec { weight 2.6 / 3.4, 28 WPM, +20 dB, F6XYZ }
pub fn vr7() -> VectorSpec { 35 WPM, +20 dB, JA1ZZZ, char_gap_units 2.5, word_gap_units 5.0 }
pub fn vr8() -> VectorSpec { 30 WPM, +10 dB, DL9ABC, watterson: Some(WattersonFade::poor()) /* same constructor v5 uses */ }
```

where `single(name, wpm, snr, text, seed, jitter)` is a small private helper building a 120 s, 96 kHz, 14 MHz-centered, +12.34 kHz-offset `VectorSpec`. `VectorSpec` gains `rise_ms: f64` (default 5.0) threaded into `KeyerSpec.rise_ms` by `render_scene`.

- [ ] **Step 4: Golden tests** — `golden_vr.rs`, one test per vector, each running `manta decode --json --engine hsmm` on the written fixture (copy the shape of `golden_v1.rs`), asserting the v2 §8.2 criteria with `manta_testkit::cer::cer`; word-boundary accuracy = fraction of expected word boundaries present in the decoded text (compare `split_whitespace()` sequences, allowing the warm-up loss of the first word). Mark all eight `#[ignore = "stage-2 gate, un-ignored by Task 12"]`.

- [ ] **Step 5: `gen vr1`…`vr8`** in the CLI match; run `cargo test --workspace`; commit.

```bash
git commit -am "feat(manta-testkit): real-conditions vectors VR1-VR8 (contest speed, deep keying, clicks, co-channel, weighting, tight spacing)"
```

---

### Task 11: CPU bench and determinism CI

**Files:**
- Create: `crates/manta-decode/benches/hsmm_300_tracks.rs`; add `[[bench]] name = "hsmm_300_tracks" harness = false` and `criterion = { workspace = true }` dev-dependency to `crates/manta-decode/Cargo.toml`
- Create: `crates/manta-cli/tests/determinism_hsmm.rs`
- Modify: `.github/workflows/*.yml` (the test job runs the new test automatically; no change unless benches are gated there)

- [ ] **Step 1: Bench**

```rust
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use manta_decode::decoder::{DecodeConfig, Engine, TrackDecoder};

fn envelope(hops: usize) -> Vec<f32> {
    // 35 WPM "CQ TEST W5AU" at 40 dB depth with 4-hop ramps, repeated to `hops`.
    let pat = [1.0f32; 13].iter().chain([0.01f32; 13].iter()).chain([1.0f32; 39].iter()).chain([0.01f32; 39].iter()).copied().collect::<Vec<_>>();
    (0..hops).map(|i| pat[i % pat.len()]).collect()
}

fn bench(c: &mut Criterion) {
    let env = envelope(375 * 10);   // 10 s
    let mut g = c.benchmark_group("hsmm");
    g.throughput(Throughput::Elements((300 * env.len()) as u64));
    g.bench_function("300_tracks_10s", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for id in 0..300u32 {
                let mut cfg = DecodeConfig::default(); cfg.engine = Engine::Hsmm;
                let mut d = TrackDecoder::new(id, cfg);
                for (i, &a) in env.iter().enumerate() { total += d.push_envelope(a, i as u64 * 512).len(); }
            }
            total
        })
    });
    g.finish();
}
criterion_group!(benches, bench);
criterion_main!(benches);
```

Budget (v2 §8.4): 300 tracks × 10 s of keying must decode in ≤ 2.5 s wall on an M-series core (≤ 25 % of real time). Record the number in the stage-2 gate doc.

- [ ] **Step 2: Determinism test**

```rust
// determinism_hsmm.rs
use sha2::{Digest, Sha256};
#[test]
fn hsmm_three_runs_identical() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v8();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let mut hashes = vec![];
    for _ in 0..3 {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta")).args(["decode", "--json", "--engine", "hsmm"]).arg(dir.path().join("v8.wav")).output().unwrap();
        assert!(out.status.success());
        hashes.push(format!("{:x}", Sha256::digest(&out.stdout)));
    }
    assert!(hashes.iter().all(|h| h == &hashes[0]), "{hashes:?}");
}
```

(`sha2` as a dev-dependency of `manta-cli` if not already present; otherwise compare the bytes directly.)

- [ ] **Step 3: Run both; commit**

```bash
git commit -am "test(manta-decode): hsmm CPU bench and 3-run determinism gate"
```

---

### Task 12: Stage-2 gate

**Files:**
- Create: `docs/DECISIONS/2026-09-XX-decode-core-v2-stage2-gate.md`
- Modify: `crates/manta-cli/tests/golden_vr.rs` (remove `#[ignore]`), `crates/manta-cli/tests/golden_v*.rs` (add `--engine hsmm` copies of V1–V10, un-ignored only if they pass)
- Modify: `README.md`, `AGENTS.md` Status (only the facts: which engine is default, measured numbers), `ROADMAP.md` (VR1–VR8 are already promoted to permanent M2 gates as of the docs PR that shipped this plan — this task records the pass/fail measurement against them, it does not decide whether they're gates)

- [ ] **Step 0: Confirm the standing decisions before measuring**

`decode.engine` stays a runtime switch permanently (never remove `Legacy`/`EdgeLegacy`); `alternatives` stays internal (do not add a dispensa field or a `manta-spot` consumer in this task — that's MAN-167); the narrowband refiner's `manta-engine` call site stays out of scope (that's MAN-168, Task 13 only builds the DSP module). VR1–VR8 are already normative in `ROADMAP.md`'s M2 section — this task measures against them, it doesn't debate whether they count.

- [ ] **Step 1: Run the gate**

```bash
cargo test --workspace --release -- --include-ignored golden_vr
cargo run --release -p manta-cli -- oracle <B2 wav> <B2 rbn csv> --engine hsmm --jsonl /tmp/oracle-hsmm.jsonl
cargo run --release -p manta-cli -- decode --json --engine hsmm <B2 wav> > /tmp/report-hsmm.json
python3 scripts/score-against-rbn.py /tmp/report-hsmm.json <B2 rbn csv> --capture-start 2025-11-29T00:00:00Z --sample-rate-hz 192000 --spotter K5TR
cargo bench -p manta-decode --bench hsmm_300_tracks
```

- [ ] **Step 2: Record** every number (oracle as-word/framed/substring/wpm-ratio by engine; K5TR-only and all-RBN recall/precision by engine; VR and V pass/fail table; bench time) in the gate doc with the commit SHA. Gate (v2 §8.4 stage 2): oracle `as_word ≥ 60 %`, `framed ≥ 40 %`; V1–V10 + VR1–VR8 pass; determinism; bench within budget.

- [ ] **Step 3: If the gate passes**: un-ignore the VR tests and the hsmm copies of V1–V10; leave `engine` default `legacy` (flipping the default is Tony's call — the gate doc ends with that question). **If it fails**: the doc lists the failing texts from the JSONL grouped by failure kind (speed, word boundary, element split/merge, garble under QRM) and the tuning knob each implicates; do not tune more than one knob per measured iteration, and record each iteration in the doc.

- [ ] **Step 4: Commit**

```bash
git add docs/DECISIONS crates/manta-cli/tests README.md AGENTS.md
git commit -m "docs(manta): decode core v2 stage-2 gate results"
```

---

### Task 13: Narrowband refiner (stage 3, optional path)

**Files:**
- Create: `crates/manta-dsp/src/refine.rs`; `pub mod refine;` in `manta-dsp/src/lib.rs`
- The engine call site (`manta-engine/src/track.rs` line ≈ 571/597 where `hop.power[k].sqrt()` is pushed) is **not** part of this task — deferred by decision (design doc §8.2) to **MAN-168**, which is the follow-up ticket already filed and cross-referenced. Do not touch `track.rs` in this task.

- [ ] **Step 1: Failing tests**

```rust
    #[test]
    fn refiner_passes_a_centered_tone_and_rejects_a_neighbor_120_hz_away() {
        let mut r = Refiner::new(30.0, 0.0);      // bw, delta channels
        let fo = 375.0;
        let mut pass = 0.0f32; let mut rej = 0.0f32;
        for t in 0..3000 {
            let tt = t as f64 / fo;
            let tone = Complex32::new(1.0, 0.0);                                  // DC = channel center
            let nb = Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 120.0 * tt) as f32);
            let a = r.push(tone); let b = r.push(nb);
            if t > 100 { pass += a; rej += b; }
        }
        assert!(pass / rej > 10.0, "pass {pass} rej {rej}");   // >= 20 dB
    }

    #[test]
    fn refiner_rotates_by_centroid_offset() {
        // A tone at +0.3 channel (+28 Hz) with delta = 0.3 must come out like DC.
        let mut r = Refiner::new(30.0, 0.3);
        let fo = 375.0; let mut acc = 0.0f32;
        for t in 0..3000 { let tt = t as f64 / fo; let x = Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 0.3 * 93.75 * tt) as f32); let a = r.push(x); if t > 100 { acc += a; } }
        assert!(acc / 2900.0 > 0.9, "mean amplitude {}", acc / 2900.0);
    }
```

- [ ] **Step 2: Implement** per v2 §3 (11-tap Hamming-windowed sinc at 375 Hz, `f64` phase accumulator wrapped each hop, `set_delta(δ)` resets nothing, channel change resets the FIR history via `reset()`).

- [ ] **Step 3: Run, commit**

```bash
git commit -am "feat(manta-dsp): per-track narrowband refiner (SPEC v2 §3), engine call site deferred to MAN-168"
```

---

### Task 14: Confidence-separation report (MAN-106 evidence)

**Files:**
- Create: `scripts/confidence-separation.py`

- [ ] **Step 1: Implement** (stdlib only): read a `manta decode --json` report and an RBN CSV (with `--spotter`), split spots into true/bogus by the same matching rule as `score-against-rbn.py` (import it), print median/quartile confidence and SNR for each class and the separation `median(true) − median(bogus)`; exit non-zero if `--min-separation X` is given and not met.

- [ ] **Step 2: Test** in `scripts/tests/test_confidence_separation.py` with a 4-spot synthetic report (two true, two bogus) asserting the printed separation.

- [ ] **Step 3: Run on tonight's report and the stage-2 report; put the numbers in the stage-2 gate doc; commit**

```bash
git commit -am "feat(scripts): confidence separation report for true vs bogus spots (MAN-106 evidence)"
```

---

## Self-review

- **Spec coverage**: v2 §1 → Task 4; §2 → Task 3; §3 → Task 13; §4.1–4.5 → Task 7; §4.6–4.10 → Task 8; §5 → Task 2; §6 → Tasks 7/8/11; §7 → Task 9; §8.1 → Task 12; §8.2 → Task 10; §8.3 → Task 6; §8.4 → Tasks 6/12/14; §9 module map matches the file table; §10 → Task 11.
- **Placeholders**: Task 10 Step 3 sketches `vr5`–`vr8` bodies in comments — acceptable because each is the `single(...)` helper with the listed field overrides and the v2 §8.2 table is normative; Task 9 Step 3 says "every v2 §7 key" — the key list is in the spec verbatim.
- **Type consistency**: `Engine` (Task 5) used by Tasks 6, 8, 9, 11; `HopEvidence` fields (Task 4) used by Tasks 5, 8; `Token::successor` signature (Task 7) used by Task 8; `Committed` (Task 8) used by `emit_committed`; `NoiseTracker::push(power, Option<f32>)` (Task 3) used by Tasks 5, 8; `TrackDecoder::push_hop(amp, power, spectral_ref_power, ts)` (Task 5) used by Task 6.
- **Out-of-scope guard**: no task edits `track.rs`, `gate.rs`, or `validator.rs`; Task 13's call site is explicitly deferred.
