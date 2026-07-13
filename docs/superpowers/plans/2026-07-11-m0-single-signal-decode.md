# M0 — Single-Signal Decode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `skimmer decode fixture.wav` prints the correct text for the SPEC §7 V1 vector (20 WPM, +20 dB SNR-in-2500-Hz, offset +12.34 kHz, W1AW, AWGN only) — end to end from a WAV file, deterministically, with CI green on Linux + macOS.

**Architecture:** Cargo workspace with 6 of the 8 planned crates (`skimmer-input`, `skimmer-dsp`, `skimmer-decode`, `skimmer-engine`, `skimmer-testkit`, `skimmer-cli`; `skimmer-spot`/`skimmer-server` arrive at M3). M0 uses a *single hardwired channel*: an FFT peak search finds the signal, a direct mix + Kaiser-prototype FIR decimator extracts one 375 Hz channel stream (the same prototype the M2 PFB will use), and the full classical decode chain (SPEC §3–§5) turns it into text. `skimmer-testkit` synthesizes the IQ ground truth.

**Tech Stack:** Rust (edition 2021, rust-version 1.85.0), `coppa-dsp` (FFT only, pinned git dep), `hound` (WAV), `num-complex`, `clap`, `serde`/`serde_json`, `rand_chacha` (testkit only), `proptest`, `tempfile`.

## Global Constraints

Copied from SPEC-decode-core.md, ARCHITECTURE.md, ROADMAP.md, CLAUDE.md. Every task's requirements implicitly include this section.

- **Determinism (SPEC §6):** NO RNG and NO wall clock anywhere in `skimmer-dsp`/`skimmer-decode`/`skimmer-engine` decode path. All timers are hop/sample counters. Any output-affecting map is `BTreeMap` or sorted `Vec`, never an iterated `HashMap`. Per-sample state is `f32`; the FIR dot product and any long accumulation run **sequentially in `f64`**. Beam tie-break: equal scores order by element-sequence lexical order, dit < dah. Softmax uses the max-subtraction trick in fixed order.
- **Timing constants:** channel output rate `fo = 375 Hz` exactly; hop period `HOP_MS = 8/3 ms`. All ms→hop conversions round **half-up** exactly once at startup: `hops = floor(ms · 0.375 + 0.5)` (SPEC §1.1). Config defaults are the SPEC §9 table — copy values exactly; do not invent constants.
- **coppa reuse boundary (SPEC §10, wiki `coppa-reuse`):** reuse `coppa-dsp::fft::FftProcessor` ONLY. The Kaiser prototype designer is NEW code in `skimmer-dsp::proto`. `coppa-dsp::agc::AdaptiveAgc` is NOT used. `coppa-channel::awgn*` is NOT used at M0 (wrong SNR convention — see Deviations below).
- **Dependency pin:** `coppa-dsp` is a git dependency on `https://github.com/HagaleTechnologies/coppa.git` pinned by `rev` (resolve `origin/main` HEAD at execution time via `git ls-remote`; record the rev in Cargo.toml and in `docs/DECISIONS/`).
- **No SoapySDR anywhere** (M0 default features must build without native libs — ROADMAP M0).
- **Licensing/metadata:** every crate `license = "MIT OR Apache-2.0"`, `edition = "2021"`, `rust-version = "1.85.0"` via workspace inheritance. LICENSE-MIT / LICENSE-APACHE files at repo root.
- **Commit `Cargo.lock`** (workspace has binaries; golden vectors depend on locked dep versions).
- **Multi-agent hygiene (CLAUDE.md):** work on a branch, push early, open a draft PR as the claim, `--force-with-lease` only, main moves only by PR merge.
- **CI:** GitHub Actions on `ubuntu-latest` + `macos-latest`: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Byte-identical decode output across 3 runs of the same binary + fixture (enforced by an integration test).
- Rustdoc comments on every public item cite the SPEC section they implement (e.g. `/// SPEC §3.2`), so the spec stays the single source of truth.

## Deviations and pinned decisions (record in `docs/DECISIONS/` in Task 16)

These are implementation decisions this plan makes where SPEC/ROADMAP are silent or conflicting. Implementers: treat these as decided.

1. **V1 is 20 WPM, not 25.** ROADMAP M0 says "25 WPM" but defers to "SPEC-decode-core §7's M0 definition", and SPEC §7 defines M0 = V1 = 20 WPM. SPEC wins; Task 16 fixes the ROADMAP line.
2. **Noise generation is testkit-local, not `coppa-channel`.** SPEC §7 says "impairments via coppa-channel (`awgn(seed)`)", but coppa's `awgn_seeded` (a) takes real `&[f32]`, not complex IQ; (b) defines SNR against measured total signal power over the full bandwidth (duty-cycle-dependent, wrong for keyed CW); (c) uses `StdRng`, which is not stable across `rand` versions. The `awgn_ref_bw` design (SPEC-watterson §6, orchestrator repo) fixes this but is **not yet in coppa**. M0 therefore implements the same formula locally in `skimmer-testkit::noise` using `ChaCha8Rng` (a specified cipher — stable forever) with a hand-rolled u64→f64 conversion and Box-Muller, so fixtures never change under dep upgrades. Migrate to coppa's `awgn_ref_bw` when it ships.
3. **SNR convention:** `amplitude = sqrt(10^(SNR_dB/10) · 2500 / fs)` against unit-power complex noise (per-component variance 0.5). SNR is defined at **key-down** (carrier amplitude), not duty-cycle-averaged.
4. **Demod init replay:** SPEC §3.2 initializes rails from the first 375 hops but doesn't say what happens to those hops' key decisions. Pin: after successful init, the buffered 375-hop window is **replayed** through the normal per-hop update so the first second of elements is decoded (required for V1 = 100% accuracy).
5. **Short leading run (debounce edge):** a sub-12 ms run with no preceding run is absorbed into the *following* run (takes the following run's polarity).
6. **Quantiles** (Q90/Q10 in SPEC §3.1–3.2) use nearest-rank on a sorted copy: `idx = ceil(q·n) − 1`, clamped to `[0, n-1]`.
7. **`+` vs `<AR>`:** SPEC §4.4 lists both `+` (char table) and `AR` (prosign) for `.-.-.`. Pin: the node's glyph is the prosign `<AR>` (SPEC resolves BT/KN collisions in favor of an explicit emission; the explicit prosign list wins). `+` is unreachable.
8. **M0 SNR estimate** (needed by SPEC §4.5's `q` factor before the §2.3 noise floor exists at M2): `SNR_2500 ≈ 20·log10(E_hi/E_lo) − 14.3 dB` from the demod rails. Documented stand-in, replaced at M2.
9. **Rail-update order** (SPEC §3.2 is silent): per hop — (1) update the rail selected by comparing against the *previous* threshold `T`; (2) enforce `E_hi ≥ 2·E_lo`; (3) recompute `T`; (4) make the key decision against the *new* `T`.
10. **Failed keying-depth init retry:** on `E_hi/E_lo < 2`, retry after 375 *new* hops using the latest 375-hop window (A_ref from that window's **first** 188 raw samples, mirroring SPEC §3.1's "first 500 ms"); earlier audio is discarded, not replayed.
11. **CharDecoded timestamp** = `start_ts` of the space run that closed the character (== end of the last mark). Flush-closed characters use the open space's `start_ts`.
12. **Gap clustering in dit units:** the Farnsworth 2-means (SPEC §4.2) clusters `u = gap_ms/μ_dit` (not raw ms) so speed drift doesn't corrupt the gap statistics. Classification thresholds are computed *before* the current gap updates the statistics.
13. **Keyer truncation:** a character is keyed iff all its segments fit inside the scene duration; the keyed-text string returned by the keyer is ground truth for CER.
14. **Extractor timing:** the extractor emits its first output only once a full L·N-sample filter window is available; output `m` carries `sample_ts = m·hop` (the window *start*), so all decoded events share a constant ~L·N/2 group-delay offset. Uniform shifts are harmless — nothing in V1 asserts absolute time.
15. **M0 fixture layout:** `<name>.wav` (stereo float32 WAV, ch0=I, ch1=Q) + `<name>.json` sidecar (`center_freq_hz`) + `<name>.manifest.json` (seeds, keyed text, expected frequency).
16. **Drift-detection anchor (found during Task 3 review, 2026-07-11):** SPEC §4.1's regime-change rule ("their mean is off *that centroid* by > 40%") is ambiguous about which centroid — the live, continuously-EMA-updated one, or a value frozen before the streak began. Task 3's original code compared against the live centroid; since every mark in the drift-detection ring has already been absorbed into that same centroid via `ClusterPair::observe`'s EMA update before the ring push, the off-centroid ratio can never grow large enough to cross the 40% bar — the mechanism was self-defeating and never fired, even for a clean 43% speed step (hand-traced: off-centroid capped at ~10%; reviewer's `step_speed_change_reinitializes` test only passed because its tolerance was loose enough for plain EMA convergence to sneak under it). Pin: the drift check anchors to the centroid as it stood *before* the current streak of 12 marks began — each ring entry now carries a `(pre_lo, pre_hi)` snapshot taken immediately before that mark's `observe()` call, and `check_drift` compares against `ring[0]`'s snapshot (the state right before the streak started) rather than `self.pair.lo`/`hi`. Verified by hand-trace: at steady state 60/180ms then 14×34ms marks, anchor≈60, off%≈43.3% > 40% → reinit fires at mark 12 as intended. The task's test tolerance was tightened from `< 6.0` to `< 1.0` ms to make sure a future regression to the live-centroid comparison fails loudly instead of silently passing via slow EMA convergence.
17. **`check_flush` must drain `Demod` before committing (found during Task 6 implementation, 2026-07-11):** `Demod` (Task 5) keeps a `held: Option<Run>` field that lags `open` by one full run for debounce confirmation (SPEC §3.3) — a completed run only surfaces via `Demod::push()`'s returned `Vec<Run>` once a FURTHER polarity flip evicts it from `held`. The last run before a track goes quiet (no further flip ever comes) is therefore stuck in `held` and invisible to the normal `push()` path. Task 6's original `check_flush` (SPEC §4.2's 7-dit forced-flush rule) read `demod.open_space_hops()`/`open_space_start_ts()` — which ARE live/un-lagged — to detect the trailing space, but then called `emit_char` directly against `self.cur_marks`, which was missing that stuck last mark. Traced on `decode("PARIS")`: flushing `S` (`...`) with only 2 of its 3 dits visible decoded as `I`, and the real EOF `finish()` call later drained the stuck mark as a spurious extra character plus a duplicate `WordBoundary` — net output `"PARII E"` instead of `"PARIS"`. Pin: `check_flush`, on deciding to force-flush, calls `self.demod.finish()` FIRST — draining both `held` and `open` — feeds any returned mark run through `process_run(r, live=true, ..)` (so it counts toward `cur_marks` and speed tracking), and deliberately does NOT separately gap-classify the returned space run (the manual `emit_char`+`WordBoundary` immediately after already decides that space's fate; classifying it too would double-emit). Verified general (not scenario-specific): `Demod::finish()` mutates only `open`/`held`, never the EMA rails/`a_ref`/`phase`, so mid-stream draining is safe and calibration survives; the held/open opposite-polarity invariant guarantees any drained `held` is always a mark when `check_flush` fires (since firing requires `open` to be a space); and the `word_flushed` guard prevents `demod.finish()` from being called twice for the same flush event.
18. **Nyquist-bin sign in `freqest::estimate_peak_hz` (found during Task 9 implementation, 2026-07-11):** SPEC §1.3's channel-index-to-Hz formula, `f(k) = f_center + ((k + N/2) mod N − N/2) · Δ`, assigns the exact Nyquist bin (`k = N/2`) the NEGATIVE label: `(N/2 + N/2) mod N = 0`, so `signed(N/2) = 0 − N/2 = −N/2`. This is the same convention as `numpy.fft.fftfreq`/`scipy.fft.fftfreq`. The task's original code used strict `if k0 > FFT_SIZE/2 { k0 - FFT_SIZE } else { k0 }`, which at exactly `k0 == FFT_SIZE/2` takes the `else` branch and returns `+N/2` — the wrong sign per SPEC. Pin: use `>=` instead of `>` in this comparison, so `k0 == FFT_SIZE/2` routes into the wrap branch. Practical impact is low (a tone at exactly ±fs/2 is physically indistinguishable at the sample level — `e^{jπn} = e^{-jπn} = (-1)^n` — so no measurement-based test can ever discriminate `>` from `>=`), but the fix keeps this estimator's Hz↔bin convention consistent with the SPEC formula the rest of the engine (channelizer, §1.4 centroid) is built on.
19. **Extractor group-delay blind zone at recording onset (found via Task 14's proptests, root-caused and fixed 2026-07-12, three attempts):** `SingleChannelExtractor`'s causal FIR prototype filter (8192 taps at fs=96kHz, frozen Task 7/SPEC §1.2 design) has group delay `(8192−1)/2/96000 = 42.661 ms`. Because the filter is causal and a recording has no history before sample 0, **no output can ever represent a true signal instant earlier than 42.661 ms into the recording** — a hard architectural floor, not a settling/tuning effect. Any part of a message's opening element(s) before that point is either fully swallowed (element dropped, e.g. `P→G`) or reduced to a corrupted decay-tail duration (e.g. a true 52.6 ms dit measured as 13.3 ms), which then corrupts `SpeedTracker`'s 5-mark bootstrap — sometimes just misclassifying dit/dah for the first character, sometimes locking `mu_dit` at its clamp floor in a structural absorbing state (`check_drift`'s CV-gate becomes unsatisfiable by the resulting mixed cluster) that cascades into garbling the entire message. Two ruled-out hypotheses, both disproven by clean-rebuild A/B evidence before the real cause was found: (a) NOT a generic "filter takes ~85ms to settle" effect — a warm-up-skip fix had no effect and broke previously-working low-WPM cases (skip landed mid-mark); (b) NOT insufficient EMA settling in `SpeedTracker` — buffering extra marks before trusting the tracker had zero measurable effect, byte-identical output with/without, even when far more marks than needed were available. **Fix**: prepend `pad_samples = extractor.filter_len()` (one full filter length) zero-valued IQ samples before the real input, run the padded array through the extractor, and — critically — feed **every** resulting output to the decoder (do NOT skip early outputs; skipping is mathematically a no-op here, since `filter_len()/hop = 4·TAPS_PER_BRANCH = 32` exactly for any supported rate, making the padded output at `m=32` reconstruct the identical envelope magnitude the unpadded pipeline already produced at its own `m=0` — proven via the extractor's NCO-mixing-then-FIR linearity: the padded and unpadded windows differ only by a global complex phase factor, which `.norm()` discards, so magnitudes are exactly identical and "skip past the padding" throws away precisely the boundary-straddling outputs that recover the message's true opening transient). Only the reported `sample_ts` is rebaselined (`sample_ts = m.saturating_sub(pad_hops) * hop`, clamped to 0 for pre-start hops) — this is reporting-only and doesn't affect decode correctness, since mark/space durations are computed from hop counts (`run.hops`), never from `sample_ts` deltas. Verified: all three originally-failing case classes (element-dropping, element-preserving misclassification, cascading garble) now decode correctly, and the full `roundtrip_iq` proptest suite (16 cases + the persisted regression) passes CER=0 across the full `10–40 WPM, 15–30 dB SNR, ±40 kHz offset` range.

## Prior art you must read before implementing

- `docs/SPEC-decode-core.md` — every constant and equation below comes from it; sections are cited per task.
- `ARCHITECTURE.md` §2 (workspace layout), §4–§5 (what M2 will grow this into).
- `wiki/pages/determinism.md`, `wiki/pages/coppa-reuse.md` — the two standing gotchas.
- coppa's `crates/coppa-dsp/src/fft.rs` — `FftProcessor::new(size)`, `forward(&[Complex32]) -> Vec<Complex32>` (unnormalized), `inverse` (scales 1/N).

## File Structure

```
skimmer/
├── Cargo.toml                      # workspace + [workspace.dependencies] + profiles
├── Cargo.lock                      # committed
├── LICENSE-MIT, LICENSE-APACHE
├── .gitignore                      # /target
├── .github/workflows/ci.yml
├── crates/
│   ├── skimmer-decode/
│   │   └── src/
│   │       ├── lib.rs              # consts (FO_HZ, HOP_MS, ms_to_hops), module decls
│   │       ├── tree.rs             # Morse table, MorseTree, Glyph, pattern_for   (SPEC §4.4 table)
│   │       ├── timing.rs           # ClusterPair, SpeedTracker, GapClassifier    (SPEC §4.1–4.2)
│   │       ├── beam.rs             # log_likelihood, decode_char, confidence     (SPEC §4.3–4.5)
│   │       ├── envelope.rs         # Demod: A_ref, rails, hysteresis, debounce   (SPEC §3)
│   │       ├── decoder.rs          # TrackDecoder glue, events_to_text           (SPEC §5)
│   │       └── events.rs           # DecoderEvent                                (SPEC §5)
│   ├── skimmer-dsp/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── proto.rs            # Kaiser windowed-sinc prototype              (SPEC §1.2)
│   │       ├── single.rs           # SingleChannelExtractor (M0 shim; superseded by pfb at M2)
│   │       └── freqest.rs          # averaged-periodogram peak search (M0 shim; superseded by §1.4 at M2)
│   ├── skimmer-input/
│   │   └── src/lib.rs              # IqSource trait, WavIqSource, Sidecar
│   ├── skimmer-testkit/
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── keyer.rs            # text → keyed envelope (raised-cosine, jitter)
│   │   │   ├── scene.rs            # signals + noise → complex IQ
│   │   │   ├── noise.rs            # awgn_ref_bw-equivalent, ChaCha8
│   │   │   ├── cer.rs              # Levenshtein CER
│   │   │   ├── wav.rs              # fixture writer (WAV + sidecar + manifest)
│   │   │   └── vectors.rs          # VectorSpec, v1()                            (SPEC §7)
│   │   └── tests/roundtrip_envelope.rs
│   ├── skimmer-engine/
│   │   ├── src/lib.rs              # decode_samples / decode_source pipeline
│   │   └── tests/roundtrip_iq.rs   # proptest, ROADMAP M0 criterion 2
│   └── skimmer-cli/
│       ├── src/main.rs             # `skimmer decode`, `skimmer gen`
│       └── tests/golden_v1.rs      # V1 acceptance + 3-run determinism
└── docs/DECISIONS/2026-07-11-m0-implementation-pins.md   (Task 15)
```

Dependency edges (all path deps inside the workspace):
`cli → engine, testkit` · `engine → input, dsp, decode` (dev: testkit) · `testkit → decode, dsp` · `dsp → coppa-dsp (git, pinned)` · `input → hound` · `decode → (serde only)`.

---

### Task 1: Branch, workspace scaffolding, CI skeleton, draft PR

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `.github/workflows/ci.yml`
- Create: `crates/skimmer-{decode,dsp,input,testkit,engine,cli}/Cargo.toml` + minimal `src/lib.rs` / `src/main.rs`

**Interfaces:**
- Produces: a building, testing, CI-green empty workspace every later task adds to. Crate names: `skimmer-decode`, `skimmer-dsp`, `skimmer-input`, `skimmer-testkit`, `skimmer-engine`, `skimmer-cli` (binary name `skimmer`).

- [ ] **Step 1: Sync and branch (multi-agent hygiene)**

```bash
cd /Users/thagale/Code/skimmer
git fetch origin && git rebase origin/main
# Check nobody has claimed M0 already:
gh pr list --state open
gh issue list --state open
git checkout -b m0-single-signal-decode
```

If an open PR already claims M0, STOP and surface it to the user.

- [ ] **Step 2: Resolve the coppa pin**

```bash
git ls-remote https://github.com/HagaleTechnologies/coppa.git refs/heads/main
```

Record the returned hash; it is `<COPPA_REV>` in every snippet below.

- [ ] **Step 3: Write the workspace `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
    "crates/skimmer-decode",
    "crates/skimmer-dsp",
    "crates/skimmer-input",
    "crates/skimmer-testkit",
    "crates/skimmer-engine",
    "crates/skimmer-cli",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
rust-version = "1.85.0"
repository = "https://github.com/HagaleTechnologies/skimmer"

[workspace.dependencies]
skimmer-decode = { path = "crates/skimmer-decode" }
skimmer-dsp = { path = "crates/skimmer-dsp" }
skimmer-input = { path = "crates/skimmer-input" }
skimmer-testkit = { path = "crates/skimmer-testkit" }
skimmer-engine = { path = "crates/skimmer-engine" }

coppa-dsp = { git = "https://github.com/HagaleTechnologies/coppa.git", rev = "<COPPA_REV>" }

anyhow = "1.0"
num-complex = "0.4"
hound = "3.5"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
rand_core = "0.9"
rand_chacha = "0.9"
proptest = "1.5"
approx = "0.5"
tempfile = "3"

# DSP in debug tests is unusably slow without this; golden V1 runs in seconds.
[profile.dev]
opt-level = 1
[profile.dev.package."*"]
opt-level = 2
```

- [ ] **Step 4: Write the six crate manifests and stub sources**

`crates/skimmer-decode/Cargo.toml`:

```toml
[package]
name = "skimmer-decode"
description = "CW keying state machine, timing, Morse decode"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }

[dev-dependencies]
approx = { workspace = true }
```

`crates/skimmer-dsp/Cargo.toml`:

```toml
[package]
name = "skimmer-dsp"
description = "Channel extraction and frequency estimation for skimmer"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
coppa-dsp = { workspace = true }
num-complex = { workspace = true }

[dev-dependencies]
approx = { workspace = true }
```

`crates/skimmer-input/Cargo.toml`:

```toml
[package]
name = "skimmer-input"
description = "IQ sources for skimmer (file playback at M0)"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
anyhow = { workspace = true }
hound = { workspace = true }
num-complex = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

`crates/skimmer-testkit/Cargo.toml`:

```toml
[package]
name = "skimmer-testkit"
description = "Synthetic CW generator and golden-vector harness"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
anyhow = { workspace = true }
hound = { workspace = true }
num-complex = { workspace = true }
rand_chacha = { workspace = true }
rand_core = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
skimmer-decode = { workspace = true }
skimmer-dsp = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }
tempfile = { workspace = true }
```

`crates/skimmer-engine/Cargo.toml`:

```toml
[package]
name = "skimmer-engine"
description = "Pipeline orchestration: input -> channel -> decoder"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
anyhow = { workspace = true }
num-complex = { workspace = true }
serde = { workspace = true }
skimmer-decode = { workspace = true }
skimmer-dsp = { workspace = true }
skimmer-input = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }
skimmer-testkit = { workspace = true }
```

`crates/skimmer-cli/Cargo.toml`:

```toml
[package]
name = "skimmer-cli"
description = "skimmer daemon and CLI"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[[bin]]
name = "skimmer"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
clap = { workspace = true }
serde_json = { workspace = true }
skimmer-engine = { workspace = true }
skimmer-testkit = { workspace = true }

[dev-dependencies]
serde_json = { workspace = true }
skimmer-testkit = { workspace = true }
tempfile = { workspace = true }
```

Each library crate gets `src/lib.rs` containing only a doc comment for now, e.g.:

```rust
//! CW keying state machine, timing, and Morse decode (SPEC-decode-core §3–§5).
```

`crates/skimmer-cli/src/main.rs`:

```rust
fn main() {
    println!("skimmer: no subcommands yet (M0 in progress)");
}
```

- [ ] **Step 5: `.gitignore`, licenses**

`.gitignore`:

```
/target
```

Copy license texts from the sibling repo (same owner, same terms):

```bash
cp ../coppa/LICENSE-MIT ../coppa/LICENSE-APACHE .
```

- [ ] **Step 6: CI workflow**

`.github/workflows/ci.yml`:

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:

env:
  CARGO_TERM_COLOR: always

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
```

Note: the coppa git dependency requires the coppa repo to be readable by CI. If `HagaleTechnologies/coppa` is private, add a `git config url."https://x-access-token:${{ secrets.COPPA_TOKEN }}@github.com/".insteadOf "https://github.com/"` step and surface that to the user — don't silently mint tokens.

- [ ] **Step 7: Verify the workspace builds and tests run**

Run: `cargo build --workspace && cargo test --workspace && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all green (zero tests, zero warnings). `Cargo.lock` now exists — commit it.

- [ ] **Step 8: Commit, push, open the draft PR (the claim)**

```bash
git add -A
git commit -m "chore: M0 workspace scaffolding (6 crates, CI, coppa pin)"
git push -u origin m0-single-signal-decode
gh pr create --draft --title "M0: single-signal decode from WAV" \
  --body "Implements ROADMAP M0 per docs/superpowers/plans/2026-07-11-m0-single-signal-decode.md. Draft = claim; do not duplicate."
```

---

### Task 2: `skimmer-decode::tree` — Morse table, tree, Glyph

**Files:**
- Create: `crates/skimmer-decode/src/tree.rs`
- Modify: `crates/skimmer-decode/src/lib.rs`

**Interfaces:**
- Consumes: nothing (leaf module).
- Produces (used by beam, decoder, testkit):
  - `pub enum Element { Dit, Dah }` (Ord: `Dit < Dah`)
  - `pub enum Prosign { Ar, Sk, As, Sn, Err }` with `pub fn token(&self) -> &'static str` (`"<AR>"` …)
  - `pub enum Glyph { Char(char), Prosign(Prosign) }` with `pub fn text_char(&self) -> Option<char>`
  - `pub type NodeId = u16;`
  - `pub struct MorseTree` with `pub fn shared() -> &'static MorseTree`, `pub const ROOT: NodeId = 0`, `pub fn child(&self, n: NodeId, e: Element) -> Option<NodeId>`, `pub fn glyph(&self, n: NodeId) -> Option<Glyph>`
  - `pub fn pattern_for(c: char) -> Option<&'static str>` (e.g. `'W'` → `".--"`, case-insensitive)
- Also add to `lib.rs`: `pub const FO_HZ: f64 = 375.0;`, `pub const HOP_MS: f64 = 8.0 / 3.0;`, `pub fn ms_to_hops(ms: f64) -> u32`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/skimmer-decode/src/tree.rs` (module skeleton + tests; implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn walk(pattern: &str) -> Option<Glyph> {
        let t = MorseTree::shared();
        let mut n = MorseTree::ROOT;
        for c in pattern.chars() {
            let e = if c == '.' { Element::Dit } else { Element::Dah };
            n = t.child(n, e)?;
        }
        t.glyph(n)
    }

    #[test]
    fn letters_digits_decode() {
        assert_eq!(walk(".-"), Some(Glyph::Char('A')));
        assert_eq!(walk("-.-."), Some(Glyph::Char('C')));
        assert_eq!(walk(".--"), Some(Glyph::Char('W')));
        assert_eq!(walk(".----"), Some(Glyph::Char('1')));
        assert_eq!(walk("-----"), Some(Glyph::Char('0')));
    }

    #[test]
    fn shared_nodes_emit_spec_glyph() {
        // SPEC §4.4: BT (-...-) emits '='; KN (-.--.) emits '('.
        assert_eq!(walk("-...-"), Some(Glyph::Char('=')));
        assert_eq!(walk("-.--."), Some(Glyph::Char('(')));
        // Pinned decision 7: .-.-. is the AR prosign (not '+').
        assert_eq!(walk(".-.-."), Some(Glyph::Prosign(Prosign::Ar)));
        assert_eq!(walk("...-.-"), Some(Glyph::Prosign(Prosign::Sk)));
        assert_eq!(walk(".-..."), Some(Glyph::Prosign(Prosign::As)));
        assert_eq!(walk("...-."), Some(Glyph::Prosign(Prosign::Sn)));
    }

    #[test]
    fn punctuation_decodes() {
        for (p, c) in [
            (".-.-.-", '.'), ("--..--", ','), ("..--..", '?'), ("-..-.", '/'),
            ("-....-", '-'), ("-.--.-", ')'), (".--.-.", '@'), ("---...", ':'),
            ("-.-.-.", ';'), (".----.", '\''), (".-..-.", '"'), ("..--.-", '_'),
            ("...-..-", '$'), ("-.-.--", '!'),
        ] {
            assert_eq!(walk(p), Some(Glyph::Char(c)), "pattern {p}");
        }
    }

    #[test]
    fn interior_nodes_are_glyphless_and_deep_paths_fall_off() {
        let t = MorseTree::shared();
        // "..-..": interior/absent paths must not panic.
        assert_eq!(walk("........"), None); // falls off the tree (max depth 7)
        assert!(t.glyph(MorseTree::ROOT).is_none());
    }

    #[test]
    fn pattern_for_encodes() {
        assert_eq!(pattern_for('W'), Some(".--"));
        assert_eq!(pattern_for('w'), Some(".--"));
        assert_eq!(pattern_for('5'), Some("....."));
        assert_eq!(pattern_for('#'), None);
    }

    #[test]
    fn element_order_is_dit_before_dah() {
        // SPEC §6.5 tie-break depends on this.
        assert!(Element::Dit < Element::Dah);
        assert!(vec![Element::Dit] < vec![Element::Dah]);
    }

    #[test]
    fn no_pattern_exceeds_seven_elements() {
        for (p, _) in TABLE {
            assert!(p.len() <= 7, "pattern {p} too long");
        }
    }
}
```

And in `lib.rs`:

```rust
#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn ms_to_hops_rounds_half_up() {
        // SPEC §1.1 single normative rounding rule; examples from SPEC §2.3–§3.3.
        assert_eq!(ms_to_hops(50.0), 19); // confirm window
        assert_eq!(ms_to_hops(12.0), 5); // debounce (4.5 -> 5)
        assert_eq!(ms_to_hops(500.0), 188); // A_ref window (187.5 -> 188)
        assert_eq!(ms_to_hops(1000.0), 375);
        assert_eq!(ms_to_hops(5000.0), 1875); // hang
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-decode`
Expected: compile error (`MorseTree` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-decode/src/lib.rs`:

```rust
//! CW keying state machine, timing, and Morse decode (SPEC-decode-core §3–§5).

pub mod beam;
pub mod decoder;
pub mod envelope;
pub mod events;
pub mod timing;
pub mod tree;

/// Channel output (envelope) rate, invariant across input rates. SPEC §1.1.
pub const FO_HZ: f64 = 375.0;
/// Hop period in milliseconds. SPEC §1.1.
pub const HOP_MS: f64 = 8.0 / 3.0;

/// The single normative ms->hop conversion: round half-up. SPEC §1.1.
pub fn ms_to_hops(ms: f64) -> u32 {
    (ms * 0.375 + 0.5).floor() as u32
}
```

(Declare all six modules now; create empty `beam.rs`, `decoder.rs`, `envelope.rs`, `events.rs`, `timing.rs` files containing only `//! stub` so the crate compiles — later tasks fill them.)

`crates/skimmer-decode/src/tree.rs`:

```rust
//! Morse code tree and glyph table. SPEC §4.4.

use std::sync::OnceLock;

/// A single keyed element. Ord derives Dit < Dah (SPEC §6.5 tie-break).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Element {
    Dit,
    Dah,
}

/// Prosigns emitted as text tokens in the JSON stream, dropped from
/// telnet-facing text. SPEC §4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Prosign {
    Ar,
    Sk,
    As,
    Sn,
    /// Operator error (........); synthesized by the beam stage, not in the tree.
    Err,
}

impl Prosign {
    pub fn token(&self) -> &'static str {
        match self {
            Prosign::Ar => "<AR>",
            Prosign::Sk => "<SK>",
            Prosign::As => "<AS>",
            Prosign::Sn => "<SN>",
            Prosign::Err => "<ERR>",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Glyph {
    Char(char),
    Prosign(Prosign),
}

impl Glyph {
    /// The plain-text rendering, or None for prosigns (dropped from text).
    pub fn text_char(&self) -> Option<char> {
        match self {
            Glyph::Char(c) => Some(*c),
            Glyph::Prosign(_) => None,
        }
    }
}

pub type NodeId = u16;

/// (pattern, glyph). SPEC §4.4 standard table + prosign terminals.
/// '+' is intentionally absent: .-.-. carries the AR prosign (pinned decision 7).
/// BT (-...-) and KN (-.--.) are the '=' and '(' nodes per SPEC.
pub(crate) const TABLE: &[(&str, Glyph)] = &[
    (".-", Glyph::Char('A')),
    ("-...", Glyph::Char('B')),
    ("-.-.", Glyph::Char('C')),
    ("-..", Glyph::Char('D')),
    (".", Glyph::Char('E')),
    ("..-.", Glyph::Char('F')),
    ("--.", Glyph::Char('G')),
    ("....", Glyph::Char('H')),
    ("..", Glyph::Char('I')),
    (".---", Glyph::Char('J')),
    ("-.-", Glyph::Char('K')),
    (".-..", Glyph::Char('L')),
    ("--", Glyph::Char('M')),
    ("-.", Glyph::Char('N')),
    ("---", Glyph::Char('O')),
    (".--.", Glyph::Char('P')),
    ("--.-", Glyph::Char('Q')),
    (".-.", Glyph::Char('R')),
    ("...", Glyph::Char('S')),
    ("-", Glyph::Char('T')),
    ("..-", Glyph::Char('U')),
    ("...-", Glyph::Char('V')),
    (".--", Glyph::Char('W')),
    ("-..-", Glyph::Char('X')),
    ("-.--", Glyph::Char('Y')),
    ("--..", Glyph::Char('Z')),
    ("-----", Glyph::Char('0')),
    (".----", Glyph::Char('1')),
    ("..---", Glyph::Char('2')),
    ("...--", Glyph::Char('3')),
    ("....-", Glyph::Char('4')),
    (".....", Glyph::Char('5')),
    ("-....", Glyph::Char('6')),
    ("--...", Glyph::Char('7')),
    ("---..", Glyph::Char('8')),
    ("----.", Glyph::Char('9')),
    (".-.-.-", Glyph::Char('.')),
    ("--..--", Glyph::Char(',')),
    ("..--..", Glyph::Char('?')),
    ("-..-.", Glyph::Char('/')),
    ("-...-", Glyph::Char('=')), // BT
    ("-....-", Glyph::Char('-')),
    ("-.--.", Glyph::Char('(')), // KN
    ("-.--.-", Glyph::Char(')')),
    (".--.-.", Glyph::Char('@')),
    ("---...", Glyph::Char(':')),
    ("-.-.-.", Glyph::Char(';')),
    (".----.", Glyph::Char('\'')),
    (".-..-.", Glyph::Char('"')),
    ("..--.-", Glyph::Char('_')),
    ("...-..-", Glyph::Char('$')),
    ("-.-.--", Glyph::Char('!')),
    (".-.-.", Glyph::Prosign(Prosign::Ar)),
    ("...-.-", Glyph::Prosign(Prosign::Sk)),
    (".-...", Glyph::Prosign(Prosign::As)),
    ("...-.", Glyph::Prosign(Prosign::Sn)),
];

#[derive(Debug, Clone, Copy)]
struct Node {
    glyph: Option<Glyph>,
    children: [Option<NodeId>; 2], // [dit, dah]
}

pub struct MorseTree {
    nodes: Vec<Node>,
}

impl MorseTree {
    pub const ROOT: NodeId = 0;

    pub fn shared() -> &'static MorseTree {
        static TREE: OnceLock<MorseTree> = OnceLock::new();
        TREE.get_or_init(MorseTree::build)
    }

    fn build() -> MorseTree {
        let mut nodes = vec![Node { glyph: None, children: [None, None] }];
        for &(pattern, glyph) in TABLE {
            let mut cur: NodeId = Self::ROOT;
            for c in pattern.chars() {
                let idx = if c == '.' { 0 } else { 1 };
                cur = match nodes[cur as usize].children[idx] {
                    Some(next) => next,
                    None => {
                        let id = nodes.len() as NodeId;
                        nodes.push(Node { glyph: None, children: [None, None] });
                        nodes[cur as usize].children[idx] = Some(id);
                        id
                    }
                };
            }
            let slot = &mut nodes[cur as usize].glyph;
            assert!(slot.is_none(), "duplicate Morse pattern {pattern}");
            *slot = Some(glyph);
        }
        MorseTree { nodes }
    }

    pub fn child(&self, n: NodeId, e: Element) -> Option<NodeId> {
        let idx = match e {
            Element::Dit => 0,
            Element::Dah => 1,
        };
        self.nodes[n as usize].children[idx]
    }

    pub fn glyph(&self, n: NodeId) -> Option<Glyph> {
        self.nodes[n as usize].glyph
    }
}

/// Encoding lookup for the testkit keyer: 'W' -> ".--" (case-insensitive).
pub fn pattern_for(c: char) -> Option<&'static str> {
    let up = c.to_ascii_uppercase();
    TABLE
        .iter()
        .find(|(_, g)| *g == Glyph::Char(up))
        .map(|(p, _)| *p)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-decode`
Expected: all tree + `ms_to_hops` tests PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(decode): Morse tree, glyph table, hop constants (SPEC §4.4, §1.1)"
```

---

### Task 3: `skimmer-decode::timing` — 2-means speed tracking + gap classification

**Files:**
- Create/replace: `crates/skimmer-decode/src/timing.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (used by decoder glue and beam):
  - `pub struct SpeedTracker` — `pub fn new() -> Self`, `pub fn on_mark(&mut self, dur_ms: f32)`, `pub fn ready(&self) -> bool`, `pub fn mu_dit_ms(&self) -> f32`, `pub fn mu_dah_ms(&self) -> f32`, `pub fn boundary_ms(&self) -> f32`, `pub fn wpm(&self) -> Option<f32>` (EMA-smoothed report value)
  - `pub enum GapClass { InterElement, InterChar, InterWord }`
  - `pub struct GapClassifier` — `pub fn new() -> Self`, `pub fn classify(&mut self, gap_ms: f32, mu_dit_ms: f32) -> GapClass`
- Constants inside this module (SPEC §9): `cluster_alpha = 0.15`, `mu_ratio_bounds = (2.2, 4.5)`, dit clamp `[20.0, 150.0]` ms, drift rule `(12 marks, CV < 0.35, off-centroid > 40 %)`, `wpm` EMA `α = 0.1`, `char_gap_dits = 2.0`, `word_gap_dits = 5.0`, Farnsworth long-gap floor `u ≥ 1.5`, activation `≥ 8` long gaps and ratio `≥ 1.8`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 20 WPM nominal: dit 60 ms, dah 180 ms.
    fn feed(t: &mut SpeedTracker, durs: &[f32]) {
        for &d in durs {
            t.on_mark(d);
        }
    }

    #[test]
    fn initializes_bimodal_after_five_marks() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]); // C-ish opening
        assert!(t.ready());
        assert!((t.mu_dit_ms() - 60.0).abs() < 1.0, "mu_dit {}", t.mu_dit_ms());
        assert!((t.mu_dah_ms() - 180.0).abs() < 1.0);
        // B = sqrt(60*180) ~ 103.9
        assert!((t.boundary_ms() - 103.92).abs() < 0.5);
        assert!((t.wpm().unwrap() - 20.0).abs() < 0.5);
    }

    #[test]
    fn unimodal_init_provisional_then_reanchors() {
        // "EEE": all dits. SPEC §4.1: mu_dah = 3*mu_dit provisionally,
        // re-anchor when a mark lands >= 2*mu_dit.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 62.0, 58.0, 60.0, 61.0]);
        assert!(t.ready());
        assert!((t.mu_dah_ms() - 3.0 * t.mu_dit_ms()).abs() < 2.0);
        t.on_mark(185.0); // first real dah re-anchors
        assert!((t.mu_dah_ms() - 185.0).abs() < 0.1);
    }

    #[test]
    fn ratio_constraint_reanchors_dah() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        // Drag mu_dah down toward mu_dit with implausibly short dahs;
        // constraint 2.2..4.5 must re-anchor mu_dah = 3*mu_dit. SPEC §4.1.
        for _ in 0..40 {
            t.on_mark(115.0);
        }
        let ratio = t.mu_dah_ms() / t.mu_dit_ms();
        assert!((2.2..=4.5).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn dit_clamp_bounds_speed() {
        let mut t = SpeedTracker::new();
        feed(&mut t, &[10.0, 30.0, 10.0, 10.0, 30.0]); // 120 WPM: beyond clamp
        assert!(t.mu_dit_ms() >= 20.0); // SPEC §4.1 clamp [20, 150]
    }

    #[test]
    fn step_speed_change_reinitializes() {
        // QRQ: 20 WPM -> 35 WPM (dit 34 ms). Plain EMA can't follow a 43 % step;
        // the drift rule (12 consecutive single-cluster, CV < 0.35, off > 40 %)
        // must reinit. SPEC §4.1.
        let mut t = SpeedTracker::new();
        feed(&mut t, &[60.0, 180.0, 60.0, 60.0, 180.0]);
        for _ in 0..3 {
            feed(&mut t, &[60.0, 180.0, 60.0]);
        }
        for _ in 0..14 {
            t.on_mark(34.0); // fast dits, all far below the old dit centroid
        }
        // Tight tolerance (pinned decision 16): the drift rule must actually
        // fire and reinit from the last 5 (all ~34ms) marks — a loose
        // tolerance here previously let a self-defeating live-centroid
        // comparison pass via slow EMA convergence alone, masking that the
        // reinit path never triggered.
        assert!((t.mu_dit_ms() - 34.0).abs() < 1.0, "mu_dit {}", t.mu_dit_ms());
    }

    #[test]
    fn gap_classification_nominal() {
        let mut g = GapClassifier::new();
        let mu = 60.0;
        assert_eq!(g.classify(60.0, mu), GapClass::InterElement); // 1 dit
        assert_eq!(g.classify(180.0, mu), GapClass::InterChar); // 3 dits
        assert_eq!(g.classify(420.0, mu), GapClass::InterWord); // 7 dits
        assert_eq!(g.classify(119.0, mu), GapClass::InterElement); // < 2.0
        assert_eq!(g.classify(299.0, mu), GapClass::InterChar); // < 5.0
        assert_eq!(g.classify(300.0, mu), GapClass::InterWord); // >= 5.0
    }

    #[test]
    fn farnsworth_moves_word_threshold() {
        // Farnsworth: char gaps stretched to 6 dits, word gaps to 14 dits.
        // Nominal rule would call 6-dit gaps InterWord; after >= 8 long gaps
        // with ratio >= 1.8 the threshold becomes sqrt(6*14) ~ 9.2 dits. SPEC §4.2.
        let mut g = GapClassifier::new();
        let mu = 48.0;
        for _ in 0..5 {
            g.classify(6.0 * mu, mu);
            g.classify(14.0 * mu, mu);
        }
        assert_eq!(g.classify(6.0 * mu, mu), GapClass::InterChar);
        assert_eq!(g.classify(14.0 * mu, mu), GapClass::InterWord);
        // Element/char boundary is speed-locked, never Farnsworth-adjusted:
        assert_eq!(g.classify(1.5 * mu, mu), GapClass::InterElement);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-decode timing`
Expected: compile error (`SpeedTracker` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-decode/src/timing.rs`:

```rust
//! Online 2-means speed tracking and gap classification. SPEC §4.1–§4.2.

use std::collections::VecDeque;

const CLUSTER_ALPHA: f32 = 0.15; // SPEC §9 decode.cluster_alpha
const RATIO_MIN: f32 = 2.2; // SPEC §9 decode.mu_ratio_bounds
const RATIO_MAX: f32 = 4.5;
const DIT_CLAMP_MS: (f32, f32) = (20.0, 150.0); // SPEC §4.1 (60..8 WPM)
const WPM_ALPHA: f32 = 0.1; // SPEC §4.1 reporting EMA
const DRIFT_LEN: usize = 12; // SPEC §4.1 regime-change rule
const DRIFT_CV_MAX: f64 = 0.35;
const DRIFT_OFF_FRAC: f64 = 0.40;
const CHAR_GAP_DITS: f32 = 2.0; // SPEC §9 decode.char_gap_dits
const WORD_GAP_DITS: f32 = 5.0; // SPEC §9 decode.word_gap_dits
const FARNS_LONG_U: f32 = 1.5; // SPEC §4.2 long-gap floor
const FARNS_MIN_COUNT: u32 = 8; // SPEC §4.2 activation
const FARNS_MIN_RATIO: f32 = 1.8;

fn mean(xs: &[f32]) -> f32 {
    let mut acc = 0.0f64;
    for &x in xs {
        acc += x as f64;
    }
    (acc / xs.len() as f64) as f32
}

/// Shared 2-means machinery: init-after-5 with largest-ratio-gap split,
/// EMA centroid updates, geometric-mean boundary. SPEC §4.1 (marks) and
/// §4.2 (gaps use "the same 2-means machinery").
#[derive(Debug, Clone)]
struct ClusterPair {
    lo: f32,
    hi: f32,
    init: Vec<f32>,
    ready: bool,
    confirmed: bool,
}

impl ClusterPair {
    fn new() -> Self {
        ClusterPair { lo: 0.0, hi: 0.0, init: Vec::with_capacity(5), ready: false, confirmed: false }
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn confirmed(&self) -> bool {
        self.confirmed
    }

    fn boundary(&self) -> f32 {
        (self.lo * self.hi).sqrt()
    }

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
        if !self.confirmed && v >= 2.0 * self.lo {
            // SPEC §4.1: unconfirmed mu_dah re-anchors to the first long mark.
            self.hi = v;
            self.confirmed = true;
            return false;
        }
        if v < self.boundary() {
            self.lo += CLUSTER_ALPHA * (v - self.lo);
        } else {
            self.hi += CLUSTER_ALPHA * (v - self.hi);
        }
        false
    }

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
        } else {
            let m = mean(&s);
            self.lo = m;
            self.hi = 3.0 * m;
            self.confirmed = false;
        }
        self.ready = true;
        self.init.clear();
    }

    fn reinit_from(&mut self, vals: &[f32]) {
        self.init = vals.to_vec();
        self.ready = false;
        self.confirmed = false;
        self.initialize();
    }
}

/// Mark-duration speed tracker. SPEC §4.1.
#[derive(Debug, Clone)]
pub struct SpeedTracker {
    pair: ClusterPair,
    // (dur_ms, assigned_dit, pre_lo, pre_hi): pre_lo/pre_hi are the centroid
    // as it stood immediately BEFORE this mark's observe() call — the drift
    // check anchors to ring[0]'s snapshot (pinned decision 16), not the live
    // centroid, which the same marks have already dragged toward themselves.
    ring: VecDeque<(f32, bool, f32, f32)>,
    wpm_ema: Option<f32>,
    recent: VecDeque<f32>, // last 5 marks, reinit source
}

impl SpeedTracker {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        SpeedTracker {
            pair: ClusterPair::new(),
            ring: VecDeque::with_capacity(DRIFT_LEN),
            wpm_ema: None,
            recent: VecDeque::with_capacity(5),
        }
    }

    pub fn ready(&self) -> bool {
        self.pair.ready()
    }

    pub fn mu_dit_ms(&self) -> f32 {
        self.pair.lo
    }

    pub fn mu_dah_ms(&self) -> f32 {
        self.pair.hi
    }

    pub fn boundary_ms(&self) -> f32 {
        self.pair.boundary()
    }

    /// EMA-smoothed PARIS WPM (SPEC §4.1: 1200/mu_dit, alpha 0.1). None until ready.
    pub fn wpm(&self) -> Option<f32> {
        self.wpm_ema
    }

    pub fn on_mark(&mut self, dur_ms: f32) {
        self.recent.push_back(dur_ms);
        if self.recent.len() > 5 {
            self.recent.pop_front();
        }
        // Snapshot before observe() can drag it (pinned decision 16).
        let pre_lo = self.pair.lo;
        let pre_hi = self.pair.hi;
        let was_init = self.pair.observe(dur_ms);
        if !self.pair.ready() {
            return;
        }
        self.apply_constraints();
        if !was_init {
            let is_dit = dur_ms < self.pair.boundary();
            self.ring.push_back((dur_ms, is_dit, pre_lo, pre_hi));
            if self.ring.len() > DRIFT_LEN {
                self.ring.pop_front();
            }
            self.check_drift();
        }
        let raw = 1200.0 / self.pair.lo;
        self.wpm_ema = Some(match self.wpm_ema {
            None => raw,
            Some(w) => w + WPM_ALPHA * (raw - w),
        });
    }

    fn apply_constraints(&mut self) {
        // SPEC §4.1: clamp mu_dit to [20, 150] ms; enforce 2.2 <= ratio <= 4.5.
        self.pair.lo = self.pair.lo.clamp(DIT_CLAMP_MS.0, DIT_CLAMP_MS.1);
        let ratio = self.pair.hi / self.pair.lo;
        if !(RATIO_MIN..=RATIO_MAX).contains(&ratio) {
            self.pair.hi = 3.0 * self.pair.lo;
        }
    }

    fn check_drift(&mut self) {
        // SPEC §4.1 regime change: 12 consecutive same-cluster marks, CV < 0.35,
        // mean off that centroid by > 40 % -> reinit from the last 5 marks.
        if self.ring.len() < DRIFT_LEN {
            return;
        }
        let all_dit = self.ring.iter().all(|&(_, d, _, _)| d);
        let all_dah = self.ring.iter().all(|&(_, d, _, _)| !d);
        if !all_dit && !all_dah {
            return;
        }
        let mut acc = 0.0f64;
        for &(d, _, _, _) in &self.ring {
            acc += d as f64;
        }
        let m = acc / self.ring.len() as f64;
        let mut var = 0.0f64;
        for &(d, _, _, _) in &self.ring {
            var += (d as f64 - m) * (d as f64 - m);
        }
        let cv = (var / self.ring.len() as f64).sqrt() / m;
        // Anchor to the centroid as it stood BEFORE this streak began
        // (pinned decision 16) — the live centroid is self-defeating, since
        // the same 12 marks have already dragged it toward them.
        let (_, _, anchor_lo, anchor_hi) = self.ring[0];
        let centroid = if all_dit { anchor_lo } else { anchor_hi } as f64;
        if cv < DRIFT_CV_MAX && (m - centroid).abs() / centroid > DRIFT_OFF_FRAC {
            let vals: Vec<f32> = self.recent.iter().copied().collect();
            self.pair.reinit_from(&vals);
            self.apply_constraints();
            self.ring.clear();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapClass {
    InterElement,
    InterChar,
    InterWord,
}

/// Gap classifier with Farnsworth decoupling. SPEC §4.2.
/// Long gaps are clustered in dit units (pinned decision 12).
#[derive(Debug, Clone)]
pub struct GapClassifier {
    pair: ClusterPair,
    long_seen: u32,
}

impl GapClassifier {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        GapClassifier { pair: ClusterPair::new(), long_seen: 0 }
    }

    fn farnsworth_active(&self) -> bool {
        self.pair.ready()
            && self.pair.confirmed()
            && self.long_seen >= FARNS_MIN_COUNT
            && self.pair.hi / self.pair.lo >= FARNS_MIN_RATIO
    }

    pub fn classify(&mut self, gap_ms: f32, mu_dit_ms: f32) -> GapClass {
        let u = gap_ms / mu_dit_ms;
        // Thresholds from statistics BEFORE this gap is incorporated.
        let word_thr = if self.farnsworth_active() {
            self.pair.boundary() // sqrt(mu_cgap * mu_wgap), in dit units
        } else {
            WORD_GAP_DITS
        };
        let class = if u < CHAR_GAP_DITS {
            GapClass::InterElement
        } else if u < word_thr {
            GapClass::InterChar
        } else {
            GapClass::InterWord
        };
        if u >= FARNS_LONG_U {
            self.pair.observe(u);
            self.long_seen += 1;
        }
        class
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-decode timing`
Expected: all 8 tests PASS. If `step_speed_change_reinitializes` fails, print `t.mu_dit_ms()` per mark — the reinit should fire on the 12th consecutive fast dit; check the ring excludes init-consumed marks.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(decode): 2-means speed tracker and Farnsworth gap classifier (SPEC §4.1–4.2)"
```

---

### Task 4: `skimmer-decode::beam` — likelihoods, beam search, confidence

**Files:**
- Create/replace: `crates/skimmer-decode/src/beam.rs`

**Interfaces:**
- Consumes: `tree::{MorseTree, Element, Glyph, Prosign}`.
- Produces (used by decoder glue):
  - `pub struct BeamConfig { pub width: usize, pub sigma: f32 }` with `Default` = `{ width: 4, sigma: 0.25 }` (SPEC §9 `decode.beam_width`, `decode.timing_sigma`)
  - `pub struct CharDecode { pub glyph: Glyph, pub confidence: f32 }`
  - `pub fn log_likelihood(dur_ms: f32, mu_ms: f32, sigma: f32) -> f32`
  - `pub fn decode_char(marks_ms: &[f32], mu_dit_ms: f32, mu_dah_ms: f32, q: f32, cfg: &BeamConfig) -> Option<CharDecode>` — `None` = aborted garble (emits nothing, SPEC §4.4 point 2); `Some(Glyph::Char('?'), 0.0)` = all-glyphless (SPEC §4.4 point 4). `q` is the pre-clamped SNR quality factor (SPEC §4.5).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Glyph, Prosign};

    const CFG: BeamConfig = BeamConfig { width: 4, sigma: 0.25 };

    #[test]
    fn log_likelihood_is_zero_at_centroid_and_symmetric_in_log() {
        assert_eq!(log_likelihood(60.0, 60.0, 0.25), 0.0);
        // SPEC §4.3: ll = -(ln d - ln mu)^2 / (2 sigma^2)
        let l = log_likelihood(120.0, 60.0, 0.25);
        let expected = -(2.0f32.ln().powi(2)) / (2.0 * 0.25 * 0.25);
        assert!((l - expected).abs() < 1e-5);
        assert!((log_likelihood(30.0, 60.0, 0.25) - l).abs() < 1e-5);
    }

    #[test]
    fn clean_character_decodes_with_high_confidence() {
        // 'A' = .- at 20 WPM
        let r = decode_char(&[60.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Char('A'));
        assert!(r.confidence > 0.9, "confidence {}", r.confidence);
    }

    #[test]
    fn marginal_mark_keeps_both_hypotheses_alive() {
        // Mark at 100 ms is ambiguous between dit(60) and dah(180); the
        // second mark (clean dah) disambiguates via the tree: ".-" = A vs "--" = M.
        let r = decode_char(&[100.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        // Both A and M survive to the boundary; winner has confidence < 1.
        assert!(r.confidence < 0.999);
        assert!(matches!(r.glyph, Glyph::Char('A') | Glyph::Char('M')));
    }

    #[test]
    fn tie_breaks_dit_before_dah() {
        // Equal mu => identical scores for both branches. SPEC §6.5:
        // element-sequence lexical order, dit < dah => 'E' wins over 'T'.
        let r = decode_char(&[100.0], 100.0, 100.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Char('E'));
        assert!((r.confidence - 0.5).abs() < 1e-4); // two equal survivors
    }

    #[test]
    fn error_prosign_on_dit_run() {
        // SPEC §4.4: >= 6 dit-classified marks with no dah -> <ERR>.
        let marks = [60.0; 8];
        let r = decode_char(&marks, 60.0, 180.0, 1.0, &CFG).unwrap();
        assert_eq!(r.glyph, Glyph::Prosign(Prosign::Err));
    }

    #[test]
    fn too_long_sequence_aborts_as_garble() {
        // 8 dahs: every 8-element path falls off the tree (max depth 7) -> None.
        let marks = [180.0; 8];
        assert!(decode_char(&marks, 60.0, 180.0, 1.0, &CFG).is_none());
    }

    #[test]
    fn q_scales_confidence() {
        let hi = decode_char(&[60.0, 180.0], 60.0, 180.0, 1.0, &CFG).unwrap();
        let lo = decode_char(&[60.0, 180.0], 60.0, 180.0, 0.3, &CFG).unwrap();
        assert!((lo.confidence - 0.3 * hi.confidence).abs() < 1e-5);
    }

    #[test]
    fn empty_marks_is_none() {
        assert!(decode_char(&[], 60.0, 180.0, 1.0, &CFG).is_none());
    }

    #[test]
    fn deterministic_across_runs() {
        let marks = [100.0, 140.0, 70.0];
        let a = decode_char(&marks, 60.0, 180.0, 0.8, &CFG).unwrap();
        let b = decode_char(&marks, 60.0, 180.0, 0.8, &CFG).unwrap();
        assert_eq!(a.glyph, b.glyph);
        assert_eq!(a.confidence.to_bits(), b.confidence.to_bits());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-decode beam`
Expected: compile error (`decode_char` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-decode/src/beam.rs`:

```rust
//! Character-local beam search over the Morse tree. SPEC §4.3–§4.5, §10.3.

use crate::tree::{Element, Glyph, MorseTree, NodeId, Prosign};

#[derive(Debug, Clone)]
pub struct BeamConfig {
    /// SPEC §9 decode.beam_width
    pub width: usize,
    /// SPEC §9 decode.timing_sigma — "the riskiest constant in the spec"
    pub sigma: f32,
}

impl Default for BeamConfig {
    fn default() -> Self {
        BeamConfig { width: 4, sigma: 0.25 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CharDecode {
    pub glyph: Glyph,
    pub confidence: f32,
}

/// Log-normal mark likelihood. SPEC §4.3.
pub fn log_likelihood(dur_ms: f32, mu_ms: f32, sigma: f32) -> f32 {
    let d = dur_ms.ln() - mu_ms.ln();
    -(d * d) / (2.0 * sigma * sigma)
}

#[derive(Debug, Clone)]
struct Hyp {
    node: NodeId,
    score: f32,
    path: Vec<Element>,
}

/// Decode one character from its mark durations. Character-local: the beam
/// resets at every character boundary (SPEC §10.3); the caller owns boundary
/// detection (SPEC §4.2 gap classification).
///
/// Returns:
/// - `None`: aborted garble — every branch fell off the tree (SPEC §4.4.2),
///   or no marks. Emits nothing; caller counts it as a decode error.
/// - `Some(Glyph::Char('?'), 0.0)`: survivors exist but none carries a glyph
///   (SPEC §4.4.4).
/// - `Some(Glyph::Prosign(Err), ..)`: operator error prosign (SPEC §4.4).
pub fn decode_char(
    marks_ms: &[f32],
    mu_dit_ms: f32,
    mu_dah_ms: f32,
    q: f32,
    cfg: &BeamConfig,
) -> Option<CharDecode> {
    if marks_ms.is_empty() {
        return None;
    }
    // SPEC §4.4 error prosign: >= 6 dit-classified marks, no dah.
    let boundary = (mu_dit_ms * mu_dah_ms).sqrt();
    if marks_ms.len() >= 6 && marks_ms.iter().all(|&d| d < boundary) {
        return Some(CharDecode { glyph: Glyph::Prosign(Prosign::Err), confidence: q });
    }

    let tree = MorseTree::shared();
    let mut hyps = vec![Hyp { node: MorseTree::ROOT, score: 0.0, path: Vec::new() }];
    for &d in marks_ms {
        let mut next: Vec<Hyp> = Vec::with_capacity(hyps.len() * 2);
        for h in &hyps {
            for (e, mu) in [(Element::Dit, mu_dit_ms), (Element::Dah, mu_dah_ms)] {
                if let Some(child) = tree.child(h.node, e) {
                    let mut path = h.path.clone();
                    path.push(e);
                    next.push(Hyp {
                        node: child,
                        score: h.score + log_likelihood(d, mu, cfg.sigma),
                        path,
                    });
                }
            }
        }
        if next.is_empty() {
            return None; // all branches dropped: garble, emits nothing
        }
        // SPEC §6.5: deterministic order — score desc, then path lex (dit < dah).
        next.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        next.truncate(cfg.width);
        hyps = next;
    }

    // Boundary: drop glyphless survivors (they stay sorted).
    let survivors: Vec<&Hyp> = hyps.iter().filter(|h| tree.glyph(h.node).is_some()).collect();
    if survivors.is_empty() {
        return Some(CharDecode { glyph: Glyph::Char('?'), confidence: 0.0 });
    }
    // SPEC §4.5 softmax with max-subtraction, fixed order; winner is survivors[0].
    let smax = survivors[0].score;
    let mut denom = 0.0f32;
    for h in &survivors {
        denom += (h.score - smax).exp();
    }
    let confidence = (1.0 / denom) * q;
    Some(CharDecode { glyph: tree.glyph(survivors[0].node).unwrap(), confidence })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-decode beam`
Expected: all 9 tests PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(decode): character-local beam search with confidence (SPEC §4.3-4.5)"
```

---

### Task 5: `skimmer-decode::envelope` — Demod (normalization, rails, hysteresis, debounce)

**Files:**
- Create/replace: `crates/skimmer-decode/src/envelope.rs`

**Interfaces:**
- Consumes: `crate::ms_to_hops`, `crate::HOP_MS`.
- Produces (used by decoder glue):
  - `pub struct DemodConfig { pub hyst_up: f32, pub hyst_down: f32, pub debounce_ms: f64, pub tau_lo_ms: f64, pub tau_hi_init_ms: f64, pub tau_hi_bounds_ms: (f64, f64) }` with `Default` = SPEC §9 values `{ 1.25, 0.80, 12.0, 500.0, 200.0, (100.0, 400.0) }`
  - `pub struct Run { pub mark: bool, pub start_ts: u64, pub hops: u32 }`
  - `pub struct Demod` —
    - `pub fn new(cfg: DemodConfig) -> Self`
    - `pub fn push(&mut self, a_raw: f32, sample_ts: u64) -> Vec<Run>` (completed, debounced runs; `sample_ts` is the input-stream sample counter of this hop)
    - `pub fn finish(&mut self) -> Vec<Run>` (EOF flush)
    - `pub fn set_dit_ms(&mut self, dit_ms: f32)` (retunes τ_hi = clamp(5·dit, 100, 400) ms, SPEC §3.2)
    - `pub fn running(&self) -> bool` (rails initialized, decisions flowing)
    - `pub fn open_space_hops(&self) -> Option<u32>`, `pub fn open_space_start_ts(&self) -> Option<u64>` (for the 7-dit flush rule)
    - `pub fn snr_2500_db(&self) -> Option<f32>` (M0 stand-in, pinned decision 8)
- Behavior pins: decisions 4, 5, 6, 9, 10 from the header.

- [ ] **Step 1: Write the failing tests**

```rust
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
        assert_eq!(first.start_ts, 0, "first mark must be recovered from replay");
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-decode envelope`
Expected: compile error (`Demod` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-decode/src/envelope.rs`:

```rust
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

/// A completed mark or space run at 375 Hz.
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
    let idx = ((q * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
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
    Init { buf: Vec<(f32, u64)>, hops_until_attempt: usize },
    Running,
}

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
    pub fn new(cfg: DemodConfig) -> Self {
        let alpha_hi = alpha_from_tau_ms(cfg.tau_hi_init_ms);
        let alpha_lo = alpha_from_tau_ms(cfg.tau_lo_ms);
        let debounce_hops = ms_to_hops(cfg.debounce_ms);
        Demod {
            cfg,
            phase: Phase::Init { buf: Vec::with_capacity(INIT_HOPS), hops_until_attempt: INIT_HOPS },
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

    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    /// SPEC §3.2: tau_hi = clamp(5 * dit_ms, 100, 400) ms once speed is tracked.
    pub fn set_dit_ms(&mut self, dit_ms: f32) {
        let tau = (5.0 * dit_ms as f64).clamp(self.cfg.tau_hi_bounds_ms.0, self.cfg.tau_hi_bounds_ms.1);
        self.alpha_hi = alpha_from_tau_ms(tau);
    }

    pub fn open_space_hops(&self) -> Option<u32> {
        match self.open {
            Some(r) if !r.mark => Some(r.hops),
            _ => None,
        }
    }

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
            Phase::Init { buf, hops_until_attempt } => {
                buf.push((a_raw, sample_ts));
                *hops_until_attempt -= 1;
                if *hops_until_attempt > 0 {
                    return out;
                }
            }
        }
        // Attempt init on the latest INIT_HOPS window.
        let Phase::Init { mut buf, .. } = std::mem::replace(&mut self.phase, Phase::Running)
        else {
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
            self.phase = Phase::Init { buf, hops_until_attempt: INIT_HOPS };
        }
        out
    }

    /// EOF flush: closes the open run and emits everything held.
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
        if !self.reest_done && (self.e_hi > 3.0 || self.e_hi < 1.0 / 3.0) && self.raw_ring.len() == AREF_HOPS {
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
                self.open = Some(Run { mark: self.key_down, start_ts: sample_ts, hops: 1 });
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
                    self.open = Some(Run { mark: self.key_down, start_ts: sample_ts, hops: 1 });
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-decode envelope`
Expected: all 7 tests PASS. Debugging notes if not:
- `clean_keying_yields_alternating_runs`: dump `(e_hi, e_lo, t)` per hop; rails must settle near (1.0, 0.01).
- `debounce_merges_short_dropout`: the merged run must combine `held + short + open`; check the `hops + 1` accounting (the current hop belongs to the merged run).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(decode): dual-EMA demod with hysteresis, debounce, init replay (SPEC §3)"
```

---

### Task 6: `skimmer-decode::decoder` + `events` — TrackDecoder glue

**Files:**
- Create/replace: `crates/skimmer-decode/src/events.rs`, `crates/skimmer-decode/src/decoder.rs`

**Interfaces:**
- Consumes: `envelope::{Demod, DemodConfig, Run}`, `timing::{SpeedTracker, GapClassifier, GapClass}`, `beam::{decode_char, BeamConfig, CharDecode}`, `tree::Glyph`, `crate::HOP_MS`.
- Produces (used by engine and cli):
  - `pub enum DecoderEvent` (in `events.rs`, `#[derive(Debug, Clone, PartialEq, serde::Serialize)]`, `#[serde(tag = "event")]`):
    - `CharDecoded { track_id: u32, sample_ts: u64, glyph: Glyph, confidence: f32 }`
    - `WordBoundary { track_id: u32, sample_ts: u64 }`
    - `SpeedUpdate { track_id: u32, wpm: f32 }`
    - `TrackMeta { track_id: u32, snr_2500_db: f32, freq_hz: f64 }`
  - `pub struct DecodeConfig { pub demod: DemodConfig, pub beam: BeamConfig, pub flush_gap_dits: f32 }` with `Default` (flush = 7.0, SPEC §9 `decode.flush_gap_dits`)
  - `pub struct TrackDecoder` — `pub fn new(track_id: u32, cfg: DecodeConfig) -> Self`, `pub fn set_freq_hz(&mut self, hz: f64)`, `pub fn push_envelope(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent>`, `pub fn finish(&mut self) -> Vec<DecoderEvent>`
  - `pub fn events_to_text(events: &[DecoderEvent]) -> String` — chars joined, `WordBoundary` → single space, prosigns dropped (SPEC §4.4), trimmed.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::pattern_for;

    /// Render text as an ideal rectangular envelope at 375 Hz.
    /// 25 WPM => dit = 48 ms = 18 hops exactly (no rounding error).
    fn rect_envelope(text: &str, dit_hops: u32) -> Vec<f32> {
        let mut env = Vec::new();
        let mut push = |level: f32, hops: u32| {
            for _ in 0..hops {
                env.push(level);
            }
        };
        let words: Vec<&str> = text.split_whitespace().collect();
        for (wi, word) in words.iter().enumerate() {
            let chars: Vec<char> = word.chars().collect();
            for (ci, c) in chars.iter().enumerate() {
                let pat = pattern_for(*c).unwrap();
                let els: Vec<char> = pat.chars().collect();
                for (ei, e) in els.iter().enumerate() {
                    push(1.0, if *e == '.' { dit_hops } else { 3 * dit_hops });
                    if ei < els.len() - 1 {
                        push(0.0, dit_hops);
                    }
                }
                if ci < chars.len() - 1 {
                    push(0.0, 3 * dit_hops);
                }
            }
            if wi < words.len() - 1 {
                push(0.0, 7 * dit_hops);
            }
        }
        push(0.0, 8 * dit_hops); // tail so the last word flushes by timeout
        env
    }

    fn decode(text: &str) -> (String, Vec<DecoderEvent>) {
        let env = rect_envelope(text, 18);
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        dec.set_freq_hz(14_012_340.0);
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        (events_to_text(&events), events)
    }

    #[test]
    fn decodes_single_word() {
        let (text, _) = decode("PARIS");
        assert_eq!(text, "PARIS");
    }

    #[test]
    fn decodes_words_with_boundaries() {
        let (text, _) = decode("CQ CQ DE W1AW");
        assert_eq!(text, "CQ CQ DE W1AW");
    }

    #[test]
    fn first_characters_are_not_lost() {
        // Tracker init consumes the first 5 marks; the pending-run buffer must
        // decode them retroactively.
        let (text, _) = decode("CQ TEST");
        assert!(text.starts_with("CQ"), "got {text:?}");
    }

    #[test]
    fn emits_speed_and_meta_events() {
        let (_, events) = decode("CQ CQ DE W1AW W1AW K");
        assert!(events.iter().any(|e| matches!(e, DecoderEvent::SpeedUpdate { wpm, .. } if (*wpm - 25.0).abs() < 3.0)));
        assert!(events
            .iter()
            .any(|e| matches!(e, DecoderEvent::TrackMeta { freq_hz, .. } if *freq_hz == 14_012_340.0)));
    }

    #[test]
    fn trailing_word_flushes_by_timeout_not_eof() {
        // The 7-dit rule (SPEC §4.2) must close "K" before the stream ends.
        let env = rect_envelope("CQ K", 18);
        let cut = env.len() - 18; // stop 1 dit short of the synthetic tail's end
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env[..cut].iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        // No finish(): the flush must already have happened via the timeout.
        assert_eq!(events_to_text(&events), "CQ K");
    }

    #[test]
    fn char_timestamp_is_end_of_last_mark() {
        // Note: a lone "E" can never decode — the tracker needs 5 marks.
        // "PARIS" gives P (.--.) = 4 marks; the 5th mark (A's dit) makes the
        // tracker ready and the pending buffer drains retroactively.
        let (_, events) = decode("PARIS");
        let ts = events
            .iter()
            .find_map(|e| match e {
                DecoderEvent::CharDecoded { sample_ts, .. } => Some(*sample_ts),
                _ => None,
            })
            .unwrap();
        // P = dit(18) g(18) dah(54) g(18) dah(54) g(18) dit(18) = ends at
        // hop 198; the closing inter-char space starts there (pinned
        // decision 11: CharDecoded ts = start of the closing space run).
        assert_eq!(ts, 198 * 256);
    }

    #[test]
    fn text_assembly_drops_prosigns_and_collapses_spaces() {
        use crate::tree::{Glyph, Prosign};
        let ev = vec![
            DecoderEvent::CharDecoded { track_id: 1, sample_ts: 0, glyph: Glyph::Char('A'), confidence: 1.0 },
            DecoderEvent::WordBoundary { track_id: 1, sample_ts: 1 },
            DecoderEvent::CharDecoded { track_id: 1, sample_ts: 2, glyph: Glyph::Prosign(Prosign::Ar), confidence: 1.0 },
            DecoderEvent::WordBoundary { track_id: 1, sample_ts: 3 },
            DecoderEvent::CharDecoded { track_id: 1, sample_ts: 4, glyph: Glyph::Char('B'), confidence: 1.0 },
        ];
        assert_eq!(events_to_text(&ev), "A B");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-decode decoder`
Expected: compile error (`TrackDecoder` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-decode/src/events.rs`:

```rust
//! Decoder output event stream. SPEC §5.

use crate::tree::Glyph;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "event")]
pub enum DecoderEvent {
    CharDecoded { track_id: u32, sample_ts: u64, glyph: Glyph, confidence: f32 },
    WordBoundary { track_id: u32, sample_ts: u64 },
    SpeedUpdate { track_id: u32, wpm: f32 },
    TrackMeta { track_id: u32, snr_2500_db: f32, freq_hz: f64 },
}
```

`crates/skimmer-decode/src/decoder.rs`:

```rust
//! Per-track decode glue: demod -> timing -> beam -> events. SPEC §3–§5.

use crate::beam::{decode_char, BeamConfig};
use crate::envelope::{Demod, DemodConfig, Run};
use crate::events::DecoderEvent;
use crate::timing::{GapClass, GapClassifier, SpeedTracker};
use crate::HOP_MS;

#[derive(Debug, Clone)]
pub struct DecodeConfig {
    pub demod: DemodConfig,
    pub beam: BeamConfig,
    /// SPEC §9 decode.flush_gap_dits
    pub flush_gap_dits: f32,
}

// Manual impl: a derived Default would zero flush_gap_dits.
impl Default for DecodeConfig {
    fn default() -> Self {
        DecodeConfig { demod: DemodConfig::default(), beam: BeamConfig::default(), flush_gap_dits: 7.0 }
    }
}

const META_INTERVAL_HOPS: u64 = 375; // SPEC §5: TrackMeta at 1 Hz cadence
const WPM_REPORT_DELTA: f32 = 1.0; // SPEC §5: SpeedUpdate on >= 1 WPM change

pub struct TrackDecoder {
    track_id: u32,
    cfg: DecodeConfig,
    demod: Demod,
    tracker: SpeedTracker,
    gaps: GapClassifier,
    /// Runs buffered until the tracker is ready (first 5 marks). They are
    /// drained through gap classification + beam retroactively.
    pending: Vec<Run>,
    cur_marks: Vec<f32>,
    word_flushed: bool,
    last_reported_wpm: Option<f32>,
    freq_hz: f64,
    hop_count: u64,
    last_ts: u64,
    /// Decode-error counter (aborted garble characters). SPEC §4.4.
    pub garble_count: u32,
}

impl TrackDecoder {
    pub fn new(track_id: u32, cfg: DecodeConfig) -> Self {
        let demod = Demod::new(cfg.demod.clone());
        TrackDecoder {
            track_id,
            cfg,
            demod,
            tracker: SpeedTracker::new(),
            gaps: GapClassifier::new(),
            pending: Vec::new(),
            cur_marks: Vec::new(),
            word_flushed: false,
            last_reported_wpm: None,
            freq_hz: 0.0,
            hop_count: 0,
            last_ts: 0,
            garble_count: 0,
        }
    }

    pub fn set_freq_hz(&mut self, hz: f64) {
        self.freq_hz = hz;
    }

    pub fn push_envelope(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        self.last_ts = sample_ts;
        let runs = self.demod.push(a, sample_ts);
        for run in runs {
            self.on_run(run, &mut events);
        }
        self.check_flush(&mut events);
        self.hop_count += 1;
        if self.hop_count % META_INTERVAL_HOPS == 0 {
            if let Some(snr) = self.demod.snr_2500_db() {
                events.push(DecoderEvent::TrackMeta {
                    track_id: self.track_id,
                    snr_2500_db: snr,
                    freq_hz: self.freq_hz,
                });
            }
        }
        events
    }

    /// End of stream: flush the demod and any open character/word.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        for run in self.demod.finish() {
            self.on_run(run, &mut events);
        }
        if !self.cur_marks.is_empty() && self.tracker.ready() {
            let ts = self.last_ts;
            self.emit_char(ts, &mut events);
            if !self.word_flushed {
                events.push(DecoderEvent::WordBoundary { track_id: self.track_id, sample_ts: ts });
            }
        }
        events
    }

    fn on_run(&mut self, run: Run, events: &mut Vec<DecoderEvent>) {
        if !self.tracker.ready() {
            if run.mark {
                self.tracker.on_mark(run.hops as f32 * HOP_MS as f32);
            }
            self.pending.push(run);
            if self.tracker.ready() {
                // Retroactively assemble the buffered runs; their marks have
                // already fed the tracker, so tracker updates are skipped.
                let drained = std::mem::take(&mut self.pending);
                for r in drained {
                    self.process_run(r, false, events);
                }
                self.demod.set_dit_ms(self.tracker.mu_dit_ms());
            }
            return;
        }
        self.process_run(run, true, events);
    }

    fn process_run(&mut self, run: Run, live: bool, events: &mut Vec<DecoderEvent>) {
        let dur_ms = run.hops as f32 * HOP_MS as f32;
        if run.mark {
            if live {
                self.tracker.on_mark(dur_ms);
                self.demod.set_dit_ms(self.tracker.mu_dit_ms());
                if let Some(w) = self.tracker.wpm() {
                    let report = match self.last_reported_wpm {
                        None => true,
                        Some(prev) => (w - prev).abs() >= WPM_REPORT_DELTA,
                    };
                    if report {
                        self.last_reported_wpm = Some(w);
                        events.push(DecoderEvent::SpeedUpdate { track_id: self.track_id, wpm: w });
                    }
                }
            }
            self.cur_marks.push(dur_ms);
            self.word_flushed = false;
        } else {
            if self.word_flushed {
                // This gap was already handled by the 7-dit flush.
                return;
            }
            match self.gaps.classify(dur_ms, self.tracker.mu_dit_ms()) {
                GapClass::InterElement => {}
                GapClass::InterChar => self.emit_char(run.start_ts, events),
                GapClass::InterWord => {
                    self.emit_char(run.start_ts, events);
                    events.push(DecoderEvent::WordBoundary {
                        track_id: self.track_id,
                        sample_ts: run.start_ts,
                    });
                }
            }
        }
    }

    /// SPEC §4.2: a trailing space reaching 7*mu_dit forces char + word flush.
    /// Pinned decision 17: `Demod::open_space_hops`/`open_space_start_ts` are
    /// live, but `Demod` lags the actual last mark by one run in `held`
    /// (debounce confirmation, SPEC §3.3) — that mark never surfaces via the
    /// normal `push()` path if the track goes quiet before another flip.
    /// Drain `Demod::finish()` first so the stuck mark is recovered into
    /// `cur_marks` before committing the flush; the returned space run is
    /// deliberately not separately gap-classified (this flush already
    /// decides its fate) to avoid a double emission.
    fn check_flush(&mut self, events: &mut Vec<DecoderEvent>) {
        if self.word_flushed || !self.tracker.ready() || self.cur_marks.is_empty() {
            return;
        }
        if let (Some(hops), Some(ts)) = (self.demod.open_space_hops(), self.demod.open_space_start_ts()) {
            let gap_ms = hops as f32 * HOP_MS as f32;
            if gap_ms >= self.cfg.flush_gap_dits * self.tracker.mu_dit_ms() {
                for run in self.demod.finish() {
                    if run.mark {
                        self.process_run(run, true, events);
                    }
                    // The returned space run is intentionally discarded here.
                }
                self.emit_char(ts, events);
                events.push(DecoderEvent::WordBoundary { track_id: self.track_id, sample_ts: ts });
                self.word_flushed = true;
            }
        }
    }

    fn emit_char(&mut self, sample_ts: u64, events: &mut Vec<DecoderEvent>) {
        if self.cur_marks.is_empty() {
            return;
        }
        // SPEC §4.5: q = clamp(SNR_2500 / 20 dB, 0.3, 1.0)
        let q = self
            .demod
            .snr_2500_db()
            .map(|snr| (snr / 20.0).clamp(0.3, 1.0))
            .unwrap_or(1.0);
        let marks = std::mem::take(&mut self.cur_marks);
        match decode_char(&marks, self.tracker.mu_dit_ms(), self.tracker.mu_dah_ms(), q, &self.cfg.beam) {
            Some(cd) => events.push(DecoderEvent::CharDecoded {
                track_id: self.track_id,
                sample_ts,
                glyph: cd.glyph,
                confidence: cd.confidence,
            }),
            None => self.garble_count += 1,
        }
    }
}

/// Assemble plain text: chars joined, word boundaries as single spaces,
/// prosigns dropped (SPEC §4.4 telnet-facing convention).
pub fn events_to_text(events: &[DecoderEvent]) -> String {
    let mut s = String::new();
    for e in events {
        match e {
            DecoderEvent::CharDecoded { glyph, .. } => {
                if let Some(c) = glyph.text_char() {
                    s.push(c);
                }
            }
            DecoderEvent::WordBoundary { .. } => {
                if !s.is_empty() && !s.ends_with(' ') {
                    s.push(' ');
                }
            }
            _ => {}
        }
    }
    s.trim().to_string()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-decode`
Expected: whole crate green (tree + timing + beam + envelope + decoder). Debugging notes:
- `decodes_single_word` failing on the first char: check the pending-run drain happens *after* the 5th mark's `on_mark` (the 5th mark run must be in `pending` when the drain runs).
- `trailing_word_flushes_by_timeout_not_eof`: `check_flush` must run on every hop, not only on completed runs.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(decode): TrackDecoder glue and event stream (SPEC §5)"
```

---

### Task 7: `skimmer-dsp::proto` — Kaiser windowed-sinc prototype designer

**Files:**
- Create: `crates/skimmer-dsp/src/proto.rs`
- Modify: `crates/skimmer-dsp/src/lib.rs`

**Interfaces:**
- Consumes: nothing (pure math; this is the NEW code SPEC §10.1 calls out — do NOT look for it in coppa).
- Produces (used by the single-channel extractor and later the M2 PFB):
  - `pub fn design_prototype(n_channels: usize, taps_per_branch: usize) -> Vec<f32>` — length `L·N`, computed in `f64`, stored `f32`, normalized `Σh = 1`. Cutoff `f_c = Δ/2` means the sinc argument is `(i − (LN−1)/2) / N` — the design is `fs`-independent (SPEC §1.2).
  - `pub const KAISER_BETA: f64 = 7.857;` (SPEC §1.2: `β = 0.1102·(A − 8.7)`, `A = 80`)
  - `pub const TAPS_PER_BRANCH: usize = 8;` (SPEC §1.2: `L = 8`)
  - `pub(crate) fn bessel_i0(x: f64) -> f64`

- [ ] **Step 1: Write the failing property tests**

These encode SPEC §1.2's objective claims; the tap snapshot (SPEC's "pinned to 1e-7") is added in Step 5 after first green.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// |H(f)| in dB at frequency f (Hz) for an fs/N channel grid.
    /// Direct DTFT — slow but exact; test-only.
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
    fn beta_matches_spec_formula() {
        // SPEC §1.2: beta = 0.1102 * (80 - 8.7) = 7.857...
        assert!((KAISER_BETA - 0.1102 * (80.0 - 8.7)).abs() < 2e-3);
    }

    #[test]
    fn prototype_is_symmetric_and_unity_dc() {
        let h = design_prototype(1024, 8);
        assert_eq!(h.len(), 8192);
        for i in 0..h.len() / 2 {
            assert_eq!(h[i], h[h.len() - 1 - i], "tap {i} asymmetric");
        }
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        assert!((sum - 1.0).abs() < 1e-6, "DC gain {sum}");
    }

    #[test]
    fn minus_six_db_at_channel_edge() {
        // SPEC §1.2: f_c = 46.875 Hz is the -6 dB point (at fs = 96 kS/s, N = 1024).
        let h = design_prototype(1024, 8);
        let edge = response_db(&h, 46.875, 96_000.0);
        assert!((edge + 6.0).abs() < 0.6, "edge response {edge} dB");
    }

    #[test]
    fn stopband_at_least_78_db() {
        // SPEC §1.2: A = 80 dB target, stopband reached by ~107 Hz offset.
        // Assert >= 78 dB from 110 Hz out (2 dB design margin).
        let h = design_prototype(1024, 8);
        let mut worst = -300.0f64;
        let mut f = 110.0;
        while f < 48_000.0 {
            worst = worst.max(response_db(&h, f, 96_000.0));
            f += 23.0; // dense enough to catch sidelobe peaks (~11.7 Hz lobes)
        }
        assert!(worst <= -78.0, "worst stopband {worst} dB");
    }

    #[test]
    fn bessel_i0_reference_values() {
        // Abramowitz & Stegun: I0(0)=1, I0(1)=1.2660658, I0(5)=27.239871
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-12);
        assert!((bessel_i0(1.0) - 1.2660658).abs() < 1e-6);
        assert!((bessel_i0(5.0) - 27.239871).abs() < 1e-4);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-dsp proto`
Expected: compile error (`design_prototype` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-dsp/src/lib.rs`:

```rust
//! Channel extraction and frequency estimation for skimmer.
//!
//! At M0 this crate holds the Kaiser prototype designer (SPEC §1.2 — NEW code,
//! coppa-dsp has no FIR designer), a single-channel extractor shim, and an
//! FFT-peak frequency estimator. The M2 PFB replaces `single` and `freqest`.

pub mod freqest;
pub mod proto;
pub mod single;
```

(Create empty stub files for `freqest.rs`/`single.rs` with a `//! stub` line so the crate compiles.)

`crates/skimmer-dsp/src/proto.rs`:

```rust
//! PFB prototype lowpass: Kaiser-windowed sinc. SPEC §1.2, §10.1.
//! All math in f64; coefficients stored as f32. Generated once at startup.

/// SPEC §1.2: beta = 0.1102 * (A - 8.7) with A = 80 dB.
pub const KAISER_BETA: f64 = 7.857;
/// SPEC §1.2: L = 8 taps per branch.
pub const TAPS_PER_BRANCH: usize = 8;

/// Modified Bessel function of the first kind, order zero (series expansion).
pub(crate) fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let half = x / 2.0;
    let mut k = 1.0f64;
    loop {
        term *= (half / k) * (half / k);
        sum += term;
        if term < sum * 1e-16 {
            return sum;
        }
        k += 1.0;
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Design the length-L·N prototype. Cutoff f_c = Δ/2 makes the sinc argument
/// (i - center)/N, so the design depends only on N and L, not fs. SPEC §1.2.
pub fn design_prototype(n_channels: usize, taps_per_branch: usize) -> Vec<f32> {
    let len = n_channels * taps_per_branch;
    let center = (len - 1) as f64 / 2.0;
    let mut h = vec![0.0f64; len];
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut sum = 0.0f64;
    for (i, tap) in h.iter_mut().enumerate() {
        let x = (i as f64 - center) / n_channels as f64; // = 2*f_c*(i-c)/fs
        let t = 2.0 * i as f64 / (len - 1) as f64 - 1.0; // [-1, 1]
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = sinc(x) * w;
        sum += *tap;
    }
    // Unity DC gain per channel (SPEC §1.2).
    h.iter().map(|&v| (v / sum) as f32).collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-dsp proto`
Expected: all 5 PASS. If `stopband_at_least_78_db` misses by < 2 dB, that is a real design-margin conversation — check the window formula before touching the assertion, and if the assertion must move, record it in `docs/DECISIONS/` (Task 15).

- [ ] **Step 5: Pin the tap snapshot (SPEC §1.2 "first/middle/last 4 taps at N=1024 pinned to 1e-7")**

Add a dump helper and run it once:

```rust
#[test]
#[ignore = "generator for the pinned snapshot below"]
fn dump_reference_taps() {
    let h = design_prototype(1024, 8);
    let mid = h.len() / 2;
    println!("first:  {:?}", &h[0..4]);
    println!("middle: {:?}", &h[mid - 2..mid + 2]);
    println!("last:   {:?}", &h[h.len() - 4..]);
}
```

Run: `cargo test -p skimmer-dsp dump_reference_taps -- --ignored --nocapture`

Paste the printed values into a new pinned test (replace the `<...>` placeholders with the actual printed numbers — they are the cross-platform stability contract from here on):

```rust
#[test]
fn reference_taps_pinned() {
    // SPEC §1.2: pinned to 1e-7. Values generated by dump_reference_taps at
    // N=1024, L=8; any change here is a golden-vector-invalidating event.
    let h = design_prototype(1024, 8);
    let mid = h.len() / 2;
    let expect_first: [f32; 4] = [<v0>, <v1>, <v2>, <v3>];
    let expect_mid: [f32; 4] = [<m0>, <m1>, <m2>, <m3>];
    let expect_last: [f32; 4] = [<l0>, <l1>, <l2>, <l3>];
    for (a, b) in h[0..4].iter().zip(expect_first) {
        assert!((a - b).abs() < 1e-7);
    }
    for (a, b) in h[mid - 2..mid + 2].iter().zip(expect_mid) {
        assert!((a - b).abs() < 1e-7);
    }
    for (a, b) in h[h.len() - 4..].iter().zip(expect_last) {
        assert!((a - b).abs() < 1e-7);
    }
}
```

Run: `cargo test -p skimmer-dsp proto` — all green including the pinned test.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(dsp): Kaiser windowed-sinc PFB prototype designer (SPEC §1.2)"
```

---

### Task 8: `skimmer-dsp::single` — single-channel extractor (M0 shim)

**Files:**
- Create/replace: `crates/skimmer-dsp/src/single.rs`

**Interfaces:**
- Consumes: `proto::{design_prototype, TAPS_PER_BRANCH}`, `num_complex::Complex32`.
- Produces (used by engine):
  - `pub struct SingleChannelExtractor` —
    - `pub fn new(fs: f64, offset_hz: f64) -> Result<Self, String>` — errors unless `fs/93.75` is a power of two (the SPEC §1.1 table rates)
    - `pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32>` — streaming; returns 375 Hz channel samples as they become available
    - `pub fn hop(&self) -> usize` — input samples per output sample (`N/4`)
- This is one PFB channel computed directly: mix by `−offset`, convolve with the §1.2 prototype, decimate by `hop = N/4`. It validates the prototype early and is replaced by `pfb.rs` at M2. NCO phase is computed per-sample from the absolute sample index in `f64` (no recurrence drift); the FIR dot product accumulates re/im **sequentially in f64** (SPEC §6.4).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / FS;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    /// Steady-state output magnitudes (skip the filter warm-up).
    fn steady(ext: &mut SingleChannelExtractor, iq: &[Complex32]) -> Vec<f32> {
        let out = ext.process(iq);
        out[40..].iter().map(|c| c.norm()).collect()
    }

    #[test]
    fn rejects_non_table_rate() {
        assert!(SingleChannelExtractor::new(44_100.0, 0.0).is_err());
        assert!(SingleChannelExtractor::new(96_000.0, 0.0).is_ok());
        assert!(SingleChannelExtractor::new(192_000.0, 0.0).is_ok());
    }

    #[test]
    fn output_rate_is_fs_over_hop() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        assert_eq!(ext.hop(), 256); // N = 1024, hop = N/4
        let out = ext.process(&tone(12_340.0, 96_000, 1.0)); // 1 s
        // 1 s of input -> ~375 outputs (minus warm-up of ~LN/hop = 32 hops).
        assert!((340..=375).contains(&out.len()), "{} outputs", out.len());
    }

    #[test]
    fn on_channel_tone_passes_at_unity() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0, 192_000, 0.5));
        for m in &mags {
            assert!((m - 0.5).abs() < 0.01, "passband magnitude {m}");
        }
    }

    #[test]
    fn tone_150hz_away_is_rejected_by_80db() {
        // SPEC §1.2: alias rejection >= 80 dB from 1.15 channels (~108 Hz) away.
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0 + 150.0, 192_000, 1.0));
        for m in &mags {
            assert!(*m < 2e-4, "stopband leak {m}"); // -74 dB, slack for f32
        }
    }

    #[test]
    fn channel_edge_is_minus_6_db() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0 + 46.875, 384_000, 1.0));
        let mean: f32 = mags.iter().sum::<f32>() / mags.len() as f32;
        // -6 dB = 0.501 in amplitude; edge tone beats at 46.875 Hz vs the
        // 375 Hz output so magnitudes are steady (complex tone at +46.875 Hz
        // in the channel), mean must sit near 0.5.
        assert!((mean - 0.5).abs() < 0.05, "edge gain {mean}");
    }

    #[test]
    fn keyed_envelope_survives_with_edges_softened() {
        // 30 ms on / 30 ms off keying (40 WPM dit rate) at channel center:
        // plateau must reach ~1.0 and troughs ~0.0 in the 375 Hz envelope.
        let n = 96_000;
        let mut iq = tone(12_340.0, n, 1.0);
        for (i, s) in iq.iter_mut().enumerate() {
            let t_ms = i as f64 * 1000.0 / FS;
            if (t_ms / 30.0) as u64 % 2 == 1 {
                *s = Complex32::new(0.0, 0.0);
            }
        }
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &iq);
        let peak = mags.iter().cloned().fold(0.0f32, f32::max);
        let trough = mags.iter().cloned().fold(f32::MAX, f32::min);
        assert!(peak > 0.9, "plateau {peak}");
        assert!(trough < 0.1, "trough {trough}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-dsp single`
Expected: compile error.

- [ ] **Step 3: Implement**

`crates/skimmer-dsp/src/single.rs`:

```rust
//! Single-channel extractor: one PFB channel computed directly (M0 shim).
//! Mix by -offset, prototype lowpass, decimate by N/4 to 375 Hz.
//! Superseded by the full PFB (SPEC §1.3) at M2.

use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use num_complex::Complex32;

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1

pub struct SingleChannelExtractor {
    taps: Vec<f32>,
    hop: usize,
    fs: f64,
    offset_hz: f64,
    /// Mixed samples not yet consumed; `read` indexes the next window start.
    /// Samples before `read` are dead and get compacted away.
    buf: Vec<Complex32>,
    read: usize,
    /// Total input samples seen (NCO phase reference).
    n_in: u64,
}

impl SingleChannelExtractor {
    pub fn new(fs: f64, offset_hz: f64) -> Result<Self, String> {
        let nf = fs / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!("unsupported sample rate {fs}: fs/93.75 must be a power of two"));
        }
        Ok(SingleChannelExtractor {
            taps: design_prototype(n, TAPS_PER_BRANCH),
            hop: n / 4,
            fs,
            offset_hz,
            buf: Vec::new(),
            read: 0,
            n_in: 0,
        })
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        // Mix to baseband. Phase from the absolute sample index in f64:
        // deterministic, no recurrence drift.
        self.buf.reserve(iq.len());
        for (k, s) in iq.iter().enumerate() {
            let n = (self.n_in + k as u64) as f64;
            let phi = -2.0 * std::f64::consts::PI * self.offset_hz * n / self.fs;
            let (sin, cos) = phi.sin_cos();
            let m = Complex32::new(cos as f32, sin as f32);
            self.buf.push(s * m);
        }
        self.n_in += iq.len() as u64;

        let ln = self.taps.len();
        let mut out = Vec::new();
        // Output y[t] = sum_i h[i] * x[t - i]; window [read, read+ln) with the
        // newest sample at the window end. Sequential f64 accumulation (SPEC §6.4).
        while self.read + ln <= self.buf.len() {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            let w = &self.buf[self.read..self.read + ln];
            for (j, x) in w.iter().enumerate() {
                let h = self.taps[ln - 1 - j] as f64;
                re += h * x.re as f64;
                im += h * x.im as f64;
            }
            out.push(Complex32::new(re as f32, im as f32));
            self.read += self.hop;
        }
        // Samples before `read` are never used again (the next window starts
        // at `read`); compact once a filter-length's worth is dead.
        if self.read >= ln {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        out
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-dsp single`
Expected: all 6 PASS. `on_channel_tone_passes_at_unity` failing at ~0.5 means the mixer sign is wrong (signal landed at the −6 dB edge); `output_rate` off by ~32 means the warm-up accounting differs — adjust the test only if the total count is right (`(len − LN)/hop + 1`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(dsp): single-channel extractor shim (mix + prototype FIR, 375 Hz out)"
```

---

### Task 9: `skimmer-dsp::freqest` — averaged-periodogram peak search (M0 shim)

**Files:**
- Create/replace: `crates/skimmer-dsp/src/freqest.rs`

**Interfaces:**
- Consumes: `coppa_dsp::fft::FftProcessor` (the ONE coppa reuse at M0), `num_complex::Complex32`.
- Produces (used by engine): `pub fn estimate_peak_hz(iq: &[Complex32], fs: f64) -> Option<f64>` — Hann-windowed 8192-point periodograms, 50 % overlap, averaged over the first `min(len, 10 s)`, quadratic interpolation on dB powers (same δ formula and clamp as SPEC §1.4). Returns `None` if fewer than 8192 samples. Replaced by the PFB + §1.4 centroid at M2; V1's "freq error ≤ 10 Hz" is asserted against this estimator at M0.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / FS;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn finds_positive_offset_within_2hz() {
        let est = estimate_peak_hz(&tone(12_340.0, 96_000), FS).unwrap();
        assert!((est - 12_340.0).abs() < 2.0, "est {est}");
    }

    #[test]
    fn finds_negative_offset_within_2hz() {
        let est = estimate_peak_hz(&tone(-20_000.0, 96_000), FS).unwrap();
        assert!((est + 20_000.0).abs() < 2.0, "est {est}");
    }

    #[test]
    fn off_bin_tone_interpolates() {
        // Half-bin offset (bin width 11.72 Hz) is the worst case for the
        // parabolic interpolator.
        let f = 12_340.0 + 5.86;
        let est = estimate_peak_hz(&tone(f, 192_000), FS).unwrap();
        assert!((est - f).abs() < 3.0, "est {est}");
    }

    #[test]
    fn keyed_tone_still_within_3hz() {
        // 50 % duty keying (60 ms period) spreads the line; the average
        // spectrum keeps the carrier dominant.
        let mut iq = tone(12_340.0, 192_000);
        for (i, s) in iq.iter_mut().enumerate() {
            let t_ms = i as f64 * 1000.0 / FS;
            if (t_ms / 60.0) as u64 % 2 == 1 {
                *s = Complex32::new(0.0, 0.0);
            }
        }
        let est = estimate_peak_hz(&iq, FS).unwrap();
        assert!((est - 12_340.0).abs() < 3.0, "est {est}");
    }

    #[test]
    fn too_short_input_is_none() {
        assert!(estimate_peak_hz(&tone(1000.0, 4096), FS).is_none());
    }

    #[test]
    fn deterministic() {
        let iq = tone(12_340.0, 96_000);
        let a = estimate_peak_hz(&iq, FS).unwrap();
        let b = estimate_peak_hz(&iq, FS).unwrap();
        assert_eq!(a.to_bits(), b.to_bits());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-dsp freqest`
Expected: compile error.

- [ ] **Step 3: Implement**

`crates/skimmer-dsp/src/freqest.rs`:

```rust
//! M0 frequency finder: averaged periodogram + parabolic interpolation.
//! Uses the same dB-domain delta formula and clamp as SPEC §1.4. Superseded
//! by the PFB detector + track centroid at M2.

use coppa_dsp::fft::FftProcessor;
use num_complex::Complex32;

const FFT_SIZE: usize = 8192;
const FRAME_HOP: usize = FFT_SIZE / 2; // 50 % overlap
const MAX_SECONDS: f64 = 10.0;

pub fn estimate_peak_hz(iq: &[Complex32], fs: f64) -> Option<f64> {
    let n_use = iq.len().min((fs * MAX_SECONDS) as usize);
    if n_use < FFT_SIZE {
        return None;
    }
    let fft = FftProcessor::new(FFT_SIZE);
    // Hann window, precomputed in f64, applied in f32.
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (FFT_SIZE - 1) as f64).cos());
            w as f32
        })
        .collect();

    let mut psd = vec![0.0f64; FFT_SIZE];
    let mut start = 0;
    let mut frames = 0u32;
    let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    while start + FFT_SIZE <= n_use {
        for (b, (x, w)) in buf.iter_mut().zip(iq[start..start + FFT_SIZE].iter().zip(&window)) {
            *b = x * w;
        }
        let spec = fft.forward(&buf);
        for (k, s) in spec.iter().enumerate() {
            psd[k] += s.norm_sqr() as f64;
        }
        frames += 1;
        start += FRAME_HOP;
    }
    debug_assert!(frames > 0);

    // Peak bin: ascending scan, strict greater-than keeps the lowest index on
    // ties (deterministic).
    let mut k0 = 0;
    for (k, &p) in psd.iter().enumerate() {
        if p > psd[k0] {
            k0 = k;
        }
    }
    // Parabolic interpolation on dB powers, SPEC §1.4 formula with clamp.
    let db = |p: f64| 10.0 * (p + 1e-30).log10();
    let pm = db(psd[(k0 + FFT_SIZE - 1) % FFT_SIZE]);
    let p0 = db(psd[k0]);
    let pp = db(psd[(k0 + 1) % FFT_SIZE]);
    let denom = pm - 2.0 * p0 + pp;
    let delta = if denom < 0.0 { (0.5 * (pm - pp) / denom).clamp(-0.5, 0.5) } else { 0.0 };

    // FFT bin order: upper half is negative frequency (SPEC §1.3 convention).
    // Pinned decision 18: >= (not >), so the exact Nyquist bin (k0 == N/2)
    // takes the SPEC formula's negative label, matching numpy.fft.fftfreq.
    let signed_bin = if k0 >= FFT_SIZE / 2 { k0 as f64 - FFT_SIZE as f64 } else { k0 as f64 };
    Some((signed_bin + delta) * fs / FFT_SIZE as f64)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-dsp`
Expected: whole crate green (proto + single + freqest).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(dsp): averaged-periodogram frequency estimator (M0 shim)"
```

---

### Task 10: `skimmer-input` — IqSource trait + WAV playback

**Files:**
- Create/replace: `crates/skimmer-input/src/lib.rs`

**Interfaces:**
- Consumes: `hound`, `serde_json`.
- Produces (used by engine, testkit tests):
  - `pub trait IqSource { fn sample_rate(&self) -> f64; fn center_freq_hz(&self) -> f64; fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize>; }` (0 = EOF)
  - `pub struct Sidecar { pub center_freq_hz: f64 }` (`serde` Deserialize+Serialize; file `<stem>.json` next to the WAV; missing file → `center_freq_hz = 0.0`)
  - `pub struct WavIqSource` — `pub fn open(path: &Path) -> anyhow::Result<Self>` (eager-loads the file, pinned M0 decision; 2-channel WAV required, ch0 = I, ch1 = Q; `Float`32 or `Int`16 accepted)
  - `pub fn read_all(src: &mut dyn IqSource) -> anyhow::Result<Vec<Complex32>>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;
    use std::io::Write;

    fn write_f32_wav(path: &std::path::Path, samples: &[Complex32], fs: u32) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: fs,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(s.re).unwrap();
            w.write_sample(s.im).unwrap();
        }
        w.finalize().unwrap();
    }

    fn samples() -> Vec<Complex32> {
        (0..1000).map(|i| Complex32::new(i as f32 / 1000.0, -(i as f32) / 2000.0)).collect()
    }

    #[test]
    fn reads_f32_wav_with_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let mut f = std::fs::File::create(dir.path().join("fix.json")).unwrap();
        f.write_all(br#"{"center_freq_hz": 14000000.0}"#).unwrap();

        let mut src = WavIqSource::open(&wav).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let all = read_all(&mut src).unwrap();
        assert_eq!(all, samples());
    }

    #[test]
    fn missing_sidecar_means_zero_center() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let src = WavIqSource::open(&wav).unwrap();
        assert_eq!(src.center_freq_hz(), 0.0);
    }

    #[test]
    fn reads_i16_wav_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix16.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 96_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        w.write_sample(16384i16).unwrap(); // I = 0.5
        w.write_sample(-16384i16).unwrap(); // Q = -0.5
        w.finalize().unwrap();
        let mut src = WavIqSource::open(&wav).unwrap();
        let all = read_all(&mut src).unwrap();
        assert_eq!(all.len(), 1);
        assert!((all[0].re - 0.5).abs() < 1e-4);
        assert!((all[0].im + 0.5).abs() < 1e-4);
    }

    #[test]
    fn mono_wav_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("mono.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 96_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        w.write_sample(0.0f32).unwrap();
        w.finalize().unwrap();
        assert!(WavIqSource::open(&wav).is_err());
    }

    #[test]
    fn read_respects_buffer_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("fix.wav");
        write_f32_wav(&wav, &samples(), 96_000);
        let mut src = WavIqSource::open(&wav).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 300];
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 300);
        assert_eq!(src.read(&mut buf).unwrap(), 100);
        assert_eq!(src.read(&mut buf).unwrap(), 0); // EOF
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-input`
Expected: compile error.

- [ ] **Step 3: Implement**

`crates/skimmer-input/src/lib.rs`:

```rust
//! IQ sources. At M0: WAV file playback only (ARCHITECTURE §3).
//! WAV layout: 2 channels, ch0 = I, ch1 = Q; Float32 or Int16.
//! Center frequency comes from a JSON sidecar `<stem>.json`.

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use std::path::Path;

pub trait IqSource {
    fn sample_rate(&self) -> f64;
    fn center_freq_hz(&self) -> f64;
    /// Fill `buf`, returning the number of samples written; 0 = EOF.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize>;
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Sidecar {
    pub center_freq_hz: f64,
}

pub struct WavIqSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl WavIqSource {
    /// Eager-loads the whole file (M0 pinned decision 15; files are <~100 MB).
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("open WAV {}", path.display()))?;
        let spec = reader.spec();
        if spec.channels != 2 {
            bail!("IQ WAV must have 2 channels (I, Q); got {}", spec.channels);
        }
        let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Float, 32) => {
                reader.samples::<f32>().collect::<Result<_, _>>()?
            }
            (hound::SampleFormat::Int, 16) => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.0))
                .collect::<Result<_, _>>()?,
            (f, b) => bail!("unsupported WAV format {f:?}/{b}-bit (need Float32 or Int16)"),
        };
        let samples = interleaved
            .chunks_exact(2)
            .map(|c| Complex32::new(c[0], c[1]))
            .collect();

        let sidecar_path = path.with_extension("json");
        let center_freq_hz = if sidecar_path.exists() {
            let text = std::fs::read_to_string(&sidecar_path)
                .with_context(|| format!("read sidecar {}", sidecar_path.display()))?;
            let sc: Sidecar = serde_json::from_str(&text)
                .with_context(|| format!("parse sidecar {}", sidecar_path.display()))?;
            sc.center_freq_hz
        } else {
            0.0
        };

        Ok(WavIqSource { samples, cursor: 0, fs: spec.sample_rate as f64, center_freq_hz })
    }
}

impl IqSource for WavIqSource {
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
}

/// Drain an IqSource to a Vec (file-mode helper).
pub fn read_all(src: &mut dyn IqSource) -> Result<Vec<Complex32>> {
    let mut all = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); 65_536];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(all);
        }
        all.extend_from_slice(&buf[..n]);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-input`
Expected: all 5 PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(input): IqSource trait and WAV+sidecar playback"
```

---

### Task 11: `skimmer-testkit::keyer` — text → keyed envelope

**Files:**
- Create: `crates/skimmer-testkit/src/keyer.rs`, `crates/skimmer-testkit/src/lib.rs`

**Interfaces:**
- Consumes: `skimmer_decode::tree::pattern_for`, `rand_chacha::ChaCha8Rng`, `rand_core::{RngCore, SeedableRng}`.
- Produces (used by scene, tests):
  - `pub struct Jitter { pub sigma: f32, pub seed: u64 }`
  - `pub struct KeyerSpec { pub wpm: f32, pub rise_ms: f64, pub jitter: Option<Jitter> }` with `pub fn new(wpm: f32) -> Self` (rise 5.0 ms — SPEC §7 preamble)
  - `pub fn key_text(text: &str, spec: &KeyerSpec, fs: f64) -> anyhow::Result<(Vec<f32>, String)>` — envelope at `fs`, plus the normalized (uppercased, whitespace-collapsed) keyed text
  - `pub fn key_text_loop(text: &str, spec: &KeyerSpec, fs: f64, duration_s: f64) -> anyhow::Result<(Vec<f32>, String)>` — repeats the payload with 7-dit word gaps until `duration_s`; keys a character only if it fits entirely (pinned decision 13); envelope zero-padded to exactly `round(duration_s·fs)` samples
  - Also in `lib.rs`: `pub(crate) fn gaussian_pair(rng: &mut ChaCha8Rng) -> (f64, f64)` — hand-rolled uniform→Box-Muller (pinned decision 2: only `next_u64()` from the RNG, so fixtures survive `rand` upgrades):

```rust
pub(crate) fn u01(rng: &mut rand_chacha::ChaCha8Rng) -> f64 {
    use rand_core::RngCore;
    // 53-bit mantissa, strictly in (0, 1): never 0 (ln-safe), never 1.
    ((rng.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9007199254740992.0)
}

pub(crate) fn gaussian_pair(rng: &mut rand_chacha::ChaCha8Rng) -> (f64, f64) {
    let u1 = u01(rng);
    let u2 = u01(rng);
    let r = (-2.0 * u1.ln()).sqrt();
    let th = std::f64::consts::TAU * u2;
    (r * th.cos(), r * th.sin())
}
```

- Timing rules (standard Morse, SPEC §7 preamble): dit = `1200/wpm` ms; dah = 3 dits; inter-element gap 1 dit; inter-character gap 3 dits; inter-word gap 7 dits. Raised-cosine edges of `rise_ms` are contained inside the element's nominal duration. Jitter multiplies each segment duration by `(1 + sigma·z)`, `z ~ N(0,1)` clamped to ±3.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn on_runs_ms(env: &[f32], fs: f64) -> Vec<f64> {
        // Durations of contiguous >0.5 stretches, in ms.
        let mut runs = Vec::new();
        let mut count = 0usize;
        for &v in env {
            if v > 0.5 {
                count += 1;
            } else if count > 0 {
                runs.push(count as f64 * 1000.0 / fs);
                count = 0;
            }
        }
        if count > 0 {
            runs.push(count as f64 * 1000.0 / fs);
        }
        runs
    }

    #[test]
    fn paris_mark_durations_at_20wpm() {
        // PARIS = .--. .- .-. .. ... => 10 dits, 4 dahs.
        let (env, text) = key_text("PARIS", &KeyerSpec::new(20.0), FS).unwrap();
        assert_eq!(text, "PARIS");
        let runs = on_runs_ms(&env, FS);
        assert_eq!(runs.len(), 14);
        // Above-half-amplitude width of a raised-cosine-edged element equals
        // its nominal duration minus rise_ms (5 ms lost at half-height).
        let dits = runs.iter().filter(|&&r| r < 100.0).count();
        let dahs = runs.iter().filter(|&&r| r >= 100.0).count();
        assert_eq!((dits, dahs), (10, 4));
        for r in &runs {
            let nominal = if *r < 100.0 { 60.0 } else { 180.0 };
            assert!((r - (nominal - 5.0)).abs() < 1.0, "run {r} vs nominal {nominal}");
        }
    }

    #[test]
    fn edges_are_raised_cosine_not_clicks() {
        let (env, _) = key_text("E", &KeyerSpec::new(20.0), FS).unwrap();
        // No sample-to-sample step exceeds what a 5 ms raised cosine allows.
        let max_step = (std::f64::consts::PI / (0.005 * FS) / 2.0) as f32 * 1.1;
        for w in env.windows(2) {
            assert!((w[1] - w[0]).abs() <= max_step, "click: {} -> {}", w[0], w[1]);
        }
    }

    #[test]
    fn word_gap_is_seven_dits() {
        let (env, _) = key_text("E E", &KeyerSpec::new(20.0), FS).unwrap();
        // envelope: dit, 7-dit gap, dit => total 9 dits = 540 ms
        assert_eq!(env.len(), (0.540 * FS) as usize);
    }

    #[test]
    fn loop_pads_to_duration_and_truncates_whole_chars() {
        // "CQ K" at 20 WPM = 43 dit units = 2.58 s per repetition, +7u gap:
        // two full repetitions end at 5.58 s, so 6 s holds exactly "CQ K CQ K"
        // and a truncated start of the third.
        let (env, text) = key_text_loop("CQ K", &KeyerSpec::new(20.0), FS, 6.0).unwrap();
        assert_eq!(env.len(), (6.0 * FS) as usize);
        assert!(text.starts_with("CQ K CQ K"), "{text}");
        // Truncation is at character granularity: no empty words.
        for word in text.split(' ') {
            assert!(!word.is_empty());
        }
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let spec = KeyerSpec { wpm: 20.0, rise_ms: 5.0, jitter: Some(Jitter { sigma: 0.08, seed: 42 }) };
        let (a, _) = key_text("PARIS", &spec, FS).unwrap();
        let (b, _) = key_text("PARIS", &spec, FS).unwrap();
        assert_eq!(a, b, "same seed must give identical envelopes");
        let runs = on_runs_ms(&a, FS);
        for r in &runs {
            let nominal = if *r < 100.0 { 60.0 } else { 180.0 };
            // 8 % sigma clamped at 3 sigma => within 24 % + edge loss
            assert!((r - (nominal - 5.0)).abs() < nominal * 0.25, "run {r}");
        }
        let (c, _) = key_text("PARIS", &KeyerSpec { jitter: Some(Jitter { sigma: 0.08, seed: 43 }), ..spec }, FS).unwrap();
        assert_ne!(a, c, "different seed must differ");
    }

    #[test]
    fn unknown_character_errors() {
        assert!(key_text("A#B", &KeyerSpec::new(20.0), FS).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-testkit keyer`
Expected: compile error.

- [ ] **Step 3: Implement**

`crates/skimmer-testkit/src/lib.rs`:

```rust
//! Synthetic CW generator and golden-vector harness (ARCHITECTURE §9).
//!
//! Determinism: all randomness is ChaCha8 seeded per fixture, consumed only
//! via `next_u64()` with hand-rolled conversions (pinned decision 2), so
//! fixtures are bit-stable across dependency upgrades and platforms.

pub mod cer;
pub mod keyer;
pub mod noise;
pub mod scene;
pub mod vectors;
pub mod wav;

pub(crate) fn u01(rng: &mut rand_chacha::ChaCha8Rng) -> f64 {
    use rand_core::RngCore;
    ((rng.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9007199254740992.0)
}

pub(crate) fn gaussian_pair(rng: &mut rand_chacha::ChaCha8Rng) -> (f64, f64) {
    let u1 = u01(rng);
    let u2 = u01(rng);
    let r = (-2.0 * u1.ln()).sqrt();
    let th = std::f64::consts::TAU * u2;
    (r * th.cos(), r * th.sin())
}
```

(Create `//! stub` files for `cer.rs`, `noise.rs`, `scene.rs`, `vectors.rs`, `wav.rs`.)

`crates/skimmer-testkit/src/keyer.rs`:

```rust
//! Text -> keyed CW envelope: raised-cosine edges, optional timing jitter.
//! SPEC §7 preamble.

use crate::gaussian_pair;
use anyhow::{bail, Result};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use skimmer_decode::tree::pattern_for;

#[derive(Debug, Clone, Copy)]
pub struct Jitter {
    /// Fractional sigma per timing segment (SPEC §7: 8 % where stated).
    pub sigma: f32,
    pub seed: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyerSpec {
    pub wpm: f32,
    /// Raised-cosine rise/fall, contained inside the element. SPEC §7: 5 ms.
    pub rise_ms: f64,
    pub jitter: Option<Jitter>,
}

impl KeyerSpec {
    pub fn new(wpm: f32) -> Self {
        KeyerSpec { wpm, rise_ms: 5.0, jitter: None }
    }
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    on: bool,
    dur_ms: f64,
}

struct SegmentBuilder {
    segs: Vec<Segment>,
    rng: Option<(ChaCha8Rng, f64)>,
}

impl SegmentBuilder {
    fn new(jitter: Option<Jitter>) -> Self {
        let rng = jitter.map(|j| (ChaCha8Rng::seed_from_u64(j.seed), j.sigma as f64));
        SegmentBuilder { segs: Vec::new(), rng }
    }

    fn push(&mut self, on: bool, nominal_ms: f64) {
        let dur_ms = match &mut self.rng {
            None => nominal_ms,
            Some((rng, sigma)) => {
                let (z, _) = gaussian_pair(rng);
                nominal_ms * (1.0 + *sigma * z.clamp(-3.0, 3.0))
            }
        };
        self.segs.push(Segment { on, dur_ms });
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase()
}

/// Append one word's segments. `unit` = dit ms. Returns Err on unknown chars.
fn push_word(b: &mut SegmentBuilder, word: &str, unit: f64) -> Result<()> {
    let chars: Vec<char> = word.chars().collect();
    for (ci, c) in chars.iter().enumerate() {
        let Some(pattern) = pattern_for(*c) else {
            bail!("character {c:?} has no Morse encoding");
        };
        let els: Vec<char> = pattern.chars().collect();
        for (ei, e) in els.iter().enumerate() {
            b.push(true, if *e == '.' { unit } else { 3.0 * unit });
            if ei < els.len() - 1 {
                b.push(false, unit);
            }
        }
        if ci < chars.len() - 1 {
            b.push(false, 3.0 * unit);
        }
    }
    Ok(())
}

fn render(segs: &[Segment], rise_ms: f64, fs: f64, total_samples: Option<usize>) -> Vec<f32> {
    let total_ms: f64 = segs.iter().map(|s| s.dur_ms).sum();
    let n = total_samples.unwrap_or((total_ms / 1000.0 * fs).round() as usize);
    let mut env = vec![0.0f32; n];
    let mut seg_idx = 0usize;
    let mut seg_start_ms = 0.0f64;
    for (i, v) in env.iter_mut().enumerate() {
        let t_ms = i as f64 * 1000.0 / fs;
        while seg_idx < segs.len() && t_ms >= seg_start_ms + segs[seg_idx].dur_ms {
            seg_start_ms += segs[seg_idx].dur_ms;
            seg_idx += 1;
        }
        if seg_idx >= segs.len() {
            break; // zero tail
        }
        let seg = segs[seg_idx];
        if !seg.on {
            continue;
        }
        let t_in = t_ms - seg_start_ms;
        let rise = rise_ms.min(seg.dur_ms / 2.0);
        let up = if t_in < rise {
            0.5 * (1.0 - (std::f64::consts::PI * t_in / rise).cos())
        } else {
            1.0
        };
        let t_rem = seg.dur_ms - t_in;
        let down = if t_rem < rise {
            0.5 * (1.0 - (std::f64::consts::PI * t_rem / rise).cos())
        } else {
            1.0
        };
        *v = up.min(down) as f32;
    }
    env
}

/// Key `text` once. Returns (envelope at fs, normalized keyed text).
pub fn key_text(text: &str, spec: &KeyerSpec, fs: f64) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let unit = 1200.0 / spec.wpm as f64;
    let mut b = SegmentBuilder::new(spec.jitter);
    let words: Vec<&str> = norm.split(' ').collect();
    for (wi, w) in words.iter().enumerate() {
        push_word(&mut b, w, unit)?;
        if wi < words.len() - 1 {
            b.push(false, 7.0 * unit);
        }
    }
    let env = render(&b.segs, spec.rise_ms, fs, None);
    Ok((env, norm))
}

/// Key `text` repeatedly (7-dit gaps between repetitions) until `duration_s`.
/// Characters are keyed only if they fit entirely (pinned decision 13).
pub fn key_text_loop(text: &str, spec: &KeyerSpec, fs: f64, duration_s: f64) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let unit = 1200.0 / spec.wpm as f64;
    let budget_ms = duration_s * 1000.0;
    let mut b = SegmentBuilder::new(spec.jitter);
    let mut keyed = String::new();
    let mut elapsed = 0.0f64;
    'outer: loop {
        let words: Vec<&str> = norm.split(' ').collect();
        for (wi, w) in words.iter().enumerate() {
            let chars: Vec<char> = w.chars().collect();
            for (ci, c) in chars.iter().enumerate() {
                // Try the character into a scratch builder to measure it.
                let mut scratch = SegmentBuilder { segs: Vec::new(), rng: b.rng.take() };
                push_word(&mut scratch, &c.to_string(), unit)?;
                let char_ms: f64 = scratch.segs.iter().map(|s| s.dur_ms).sum();
                b.rng = scratch.rng.take();
                if elapsed + char_ms > budget_ms {
                    break 'outer;
                }
                b.segs.extend(scratch.segs);
                elapsed += char_ms;
                keyed.push(*c);
                if ci < chars.len() - 1 {
                    b.push(false, 3.0 * unit);
                    elapsed += b.segs.last().unwrap().dur_ms;
                }
            }
            if wi < words.len() - 1 {
                b.push(false, 7.0 * unit);
                elapsed += b.segs.last().unwrap().dur_ms;
                keyed.push(' ');
            }
        }
        b.push(false, 7.0 * unit);
        elapsed += b.segs.last().unwrap().dur_ms;
        keyed.push(' ');
        if elapsed >= budget_ms {
            break;
        }
    }
    let n = (duration_s * fs).round() as usize;
    let env = render(&b.segs, spec.rise_ms, fs, Some(n));
    Ok((env, keyed.trim().to_string()))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-testkit keyer`
Expected: all 6 PASS. Note for `paris_mark_durations_at_20wpm`: the >0.5 width of an element with contained raised-cosine edges is `nominal − rise_ms` (half-height is reached `rise/2` in from each end... verify: half-height at `t_in = rise/2` on each side ⇒ width = `nominal − rise`). If the measured runs are `nominal − rise/2`, the edges were placed *outside* the element — fix the keyer, not the test.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(testkit): CW keyer with raised-cosine edges and seeded jitter"
```

---

### Task 12: `skimmer-testkit` — noise, scene, CER, WAV writer, V1 vector

**Files:**
- Create/replace: `crates/skimmer-testkit/src/{noise,scene,cer,wav,vectors}.rs`

**Interfaces:**
- Consumes: `keyer`, `gaussian_pair`, `hound`, `skimmer-decode` (nothing else).
- Produces (used by engine tests and cli):
  - `noise.rs`: `pub fn add_unit_awgn(samples: &mut [Complex32], seed: u64)` (unit-power complex noise: per-component σ² = 0.5; I then Q per sample, fixed order); `pub fn amplitude_for_snr_2500(snr_db: f32, fs: f64) -> f32` = `sqrt(10^(snr/10) · 2500/fs)` (pinned decision 3)
  - `scene.rs`: `pub struct SignalSpec { pub text: String, pub loop_text: bool, pub wpm: f32, pub offset_hz: f64, pub snr_2500_db: f32, pub jitter: Option<Jitter> }`; `pub const MASTER_SCALE: f32 = 0.05;`; `pub fn render_scene(signals: &[SignalSpec], fs: f64, duration_s: f64, noise_seed: Option<u64>) -> anyhow::Result<(Vec<Complex32>, Vec<String>)>` (signals summed in slice order, then noise, then `MASTER_SCALE`; returns keyed texts per signal)
  - `cer.rs`: `pub fn cer(expected: &str, decoded: &str) -> f64` (Levenshtein / expected length, both sides whitespace-normalized + uppercased); `pub fn char_accuracy(expected: &str, decoded: &str) -> f64`
  - `wav.rs`: `pub fn write_fixture(dir: &Path, name: &str, samples: &[Complex32], fs: f64, center_freq_hz: f64) -> anyhow::Result<PathBuf>` (writes `<name>.wav` float32 stereo + `<name>.json` sidecar, returns the wav path)
  - `vectors.rs`:
    - `pub struct VectorSpec { pub name: &'static str, pub fs: f64, pub duration_s: f64, pub center_freq_hz: f64, pub noise_seed: u64, pub signals: Vec<SignalSpec> }`
    - `pub fn v1() -> VectorSpec` — SPEC §7 V1: 96 kS/s, 120 s, 20 WPM, +20 dB, offset +12 340.0 Hz, `"CQ CQ DE W1AW W1AW K"` looped, no jitter, `noise_seed = 0x534B_494D_5631` ("SKIMV1"), center 14 MHz
    - `pub struct RenderedVector { pub samples: Vec<Complex32>, pub keyed_texts: Vec<String>, pub expected_freq_hz: f64 }`
    - `pub fn render(spec: &VectorSpec) -> anyhow::Result<RenderedVector>`
    - `pub fn write_fixture_set(spec: &VectorSpec, dir: &Path) -> anyhow::Result<Manifest>` — wav + sidecar + `<name>.manifest.json`; `pub struct Manifest { pub name: String, pub fs: f64, pub duration_s: f64, pub center_freq_hz: f64, pub noise_seed: u64, pub expected_freq_hz: f64, pub keyed_texts: Vec<String>, pub generator: String }` (serde Serialize + Deserialize; `generator` = `concat!("skimmer-testkit ", env!("CARGO_PKG_VERSION"))`)

- [ ] **Step 1: Write the failing tests**

In `noise.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    #[test]
    fn unit_noise_power_and_determinism() {
        let mut a = vec![Complex32::new(0.0, 0.0); 200_000];
        add_unit_awgn(&mut a, 7);
        let p: f64 = a.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / a.len() as f64;
        assert!((p - 1.0).abs() < 0.02, "noise power {p}");
        let mut b = vec![Complex32::new(0.0, 0.0); 200_000];
        add_unit_awgn(&mut b, 7);
        assert_eq!(a, b);
    }

    #[test]
    fn amplitude_formula() {
        // +20 dB in 2500 Hz at 96 kS/s: sqrt(100 * 2500/96000) = 1.6137
        let a = amplitude_for_snr_2500(20.0, 96_000.0);
        assert!((a - 1.6137).abs() < 1e-3, "{a}");
    }
}
```

In `scene.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn achieved_snr_matches_request() {
        // Measure key-down signal power vs noise power in the 2500 Hz
        // convention; must be within 0.3 dB of the request (pinned decision 3).
        let fs = 96_000.0;
        let sig = SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
        };
        let (clean, _) = render_scene(std::slice::from_ref(&sig), fs, 10.0, None).unwrap();
        let (noisy_only, _) = render_scene(&[], fs, 10.0, Some(1)).unwrap();
        // Key-down mask from the clean scene:
        let plateau = clean.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
        let keydown: Vec<f32> = clean.iter().map(|c| c.norm()).filter(|&m| m > 0.9 * plateau).collect();
        let p_sig: f64 = keydown.iter().map(|&m| (m as f64) * (m as f64)).sum::<f64>() / keydown.len() as f64;
        let p_noise: f64 =
            noisy_only.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / noisy_only.len() as f64;
        let snr_2500 = 10.0 * (p_sig / (p_noise * 2500.0 / fs)).log10();
        assert!((snr_2500 - 20.0).abs() < 0.3, "achieved SNR {snr_2500}");
    }

    #[test]
    fn scene_is_deterministic() {
        let sig = SignalSpec {
            text: "TEST".into(),
            loop_text: false,
            wpm: 25.0,
            offset_hz: -5_000.0,
            snr_2500_db: 15.0,
            jitter: Some(crate::keyer::Jitter { sigma: 0.08, seed: 9 }),
        };
        let a = render_scene(std::slice::from_ref(&sig), 96_000.0, 3.0, Some(2)).unwrap();
        let b = render_scene(std::slice::from_ref(&sig), 96_000.0, 3.0, Some(2)).unwrap();
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }
}
```

In `cer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_zero() {
        assert_eq!(cer("CQ CQ DE W1AW", "CQ CQ DE W1AW"), 0.0);
        assert_eq!(cer("CQ  CQ", "cq cq"), 0.0); // normalization
    }

    #[test]
    fn substitution_counts() {
        // 1 edit over 5 chars
        assert!((cer("PARIS", "PARIX") - 0.2).abs() < 1e-9);
    }

    #[test]
    fn insertion_and_deletion_count() {
        assert!((cer("PARIS", "PARIS5") - 0.2).abs() < 1e-9);
        assert!((cer("PARIS", "PAIS") - 0.2).abs() < 1e-9);
        assert!((char_accuracy("PARIS", "PARIS") - 1.0).abs() < 1e-9);
    }
}
```

In `vectors.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use skimmer_input::{IqSource, WavIqSource};

    #[test]
    fn v1_spec_matches_spec_table() {
        let v = v1();
        assert_eq!(v.fs, 96_000.0);
        assert_eq!(v.duration_s, 120.0);
        let s = &v.signals[0];
        assert_eq!(s.wpm, 20.0);
        assert_eq!(s.offset_hz, 12_340.0);
        assert_eq!(s.snr_2500_db, 20.0);
        assert!(s.jitter.is_none()); // V1: no jitter
        assert_eq!(s.text, "CQ CQ DE W1AW W1AW K");
    }

    #[test]
    fn fixture_roundtrips_through_wav() {
        // Short variant so the test stays fast; same code path as V1.
        let spec = VectorSpec { duration_s: 3.0, ..v1() };
        let rendered = render(&spec).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_fixture_set(&spec, dir.path()).unwrap();
        assert_eq!(manifest.expected_freq_hz, 14_012_340.0);

        let mut src = WavIqSource::open(&dir.path().join("v1.wav")).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let back = skimmer_input::read_all(&mut src).unwrap();
        assert_eq!(back, rendered.samples); // float32 WAV is lossless
    }
}
```

This test needs `skimmer-input` as a dev-dependency of `skimmer-testkit` — add it:

```toml
[dev-dependencies]
skimmer-input = { workspace = true }
```

(also add `skimmer-input = { path = "crates/skimmer-input" }` … it is already in `[workspace.dependencies]` from Task 1.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-testkit`
Expected: compile errors for the four new modules.

- [ ] **Step 3: Implement**

`crates/skimmer-testkit/src/noise.rs`:

```rust
//! Complex AWGN with the SNR-in-reference-bandwidth convention.
//! Local stand-in for coppa's future `awgn_ref_bw` (pinned decision 2):
//! coppa-channel's awgn measures duty-cycle-dependent signal power over the
//! full bandwidth and uses a version-unstable RNG — wrong for keyed CW
//! golden vectors. Migrate when coppa ships awgn_ref_bw (SPEC-watterson §6).

use crate::gaussian_pair;
use num_complex::Complex32;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

/// Add unit-total-power complex white noise (per-component variance 0.5).
/// Sampling order: one Box-Muller pair per sample, (I, Q) — fixed forever.
pub fn add_unit_awgn(samples: &mut [Complex32], seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let sigma = 0.5f64.sqrt();
    for s in samples.iter_mut() {
        let (zi, zq) = gaussian_pair(&mut rng);
        s.re += (sigma * zi) as f32;
        s.im += (sigma * zq) as f32;
    }
}

/// Key-down carrier amplitude for a requested SNR in 2500 Hz against
/// unit-power noise spread over fs (pinned decision 3; SPEC §7 quotes SNR
/// in 2500 Hz).
pub fn amplitude_for_snr_2500(snr_db: f32, fs: f64) -> f32 {
    ((10.0f64.powf(snr_db as f64 / 10.0)) * 2500.0 / fs).sqrt() as f32
}
```

`crates/skimmer-testkit/src/scene.rs`:

```rust
//! Compose keyed CW signals + noise into one IQ scene. ARCHITECTURE §9.

use crate::keyer::{key_text, key_text_loop, Jitter, KeyerSpec};
use crate::noise::{add_unit_awgn, amplitude_for_snr_2500};
use anyhow::Result;
use num_complex::Complex32;

#[derive(Debug, Clone)]
pub struct SignalSpec {
    pub text: String,
    /// true: repeat the payload for the whole scene (SPEC §7 default payload
    /// behavior); false: key once, silence after.
    pub loop_text: bool,
    pub wpm: f32,
    pub offset_hz: f64,
    pub snr_2500_db: f32,
    pub jitter: Option<Jitter>,
}

/// Headroom scale applied to signal+noise after mixing (float32 WAV; keeps
/// peaks well under 1.0 for i16 export later).
pub const MASTER_SCALE: f32 = 0.05;

/// Render signals (slice order) + optional noise, then MASTER_SCALE.
/// Returns the scene and the keyed text of each signal.
pub fn render_scene(
    signals: &[SignalSpec],
    fs: f64,
    duration_s: f64,
    noise_seed: Option<u64>,
) -> Result<(Vec<Complex32>, Vec<String>)> {
    let n = (duration_s * fs).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let mut texts = Vec::with_capacity(signals.len());
    for sig in signals {
        let spec = KeyerSpec { wpm: sig.wpm, rise_ms: 5.0, jitter: sig.jitter };
        let (env, text) = if sig.loop_text {
            key_text_loop(&sig.text, &spec, fs, duration_s)?
        } else {
            key_text(&sig.text, &spec, fs)?
        };
        texts.push(text);
        let amp = amplitude_for_snr_2500(sig.snr_2500_db, fs);
        // Phase-accumulator NCO in f64 with wrap: deterministic, no
        // large-argument sin/cos precision loss.
        let dphi = std::f64::consts::TAU * sig.offset_hz / fs;
        let mut phi = 0.0f64;
        for (i, out) in acc.iter_mut().enumerate() {
            let e = env.get(i).copied().unwrap_or(0.0) * amp;
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
    if let Some(seed) = noise_seed {
        add_unit_awgn(&mut acc, seed);
    }
    for s in &mut acc {
        *s *= MASTER_SCALE;
    }
    Ok((acc, texts))
}
```

`crates/skimmer-testkit/src/cer.rs`:

```rust
//! Character error rate: Levenshtein distance / expected length.

fn normalize(s: &str) -> Vec<char> {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
        .chars()
        .collect()
}

pub fn cer(expected: &str, decoded: &str) -> f64 {
    let e = normalize(expected);
    let d = normalize(decoded);
    if e.is_empty() {
        return if d.is_empty() { 0.0 } else { 1.0 };
    }
    // Two-row Levenshtein DP.
    let mut prev: Vec<usize> = (0..=d.len()).collect();
    let mut cur = vec![0usize; d.len() + 1];
    for i in 1..=e.len() {
        cur[0] = i;
        for j in 1..=d.len() {
            let sub = prev[j - 1] + usize::from(e[i - 1] != d[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[d.len()] as f64 / e.len() as f64
}

pub fn char_accuracy(expected: &str, decoded: &str) -> f64 {
    1.0 - cer(expected, decoded)
}
```

`crates/skimmer-testkit/src/wav.rs`:

```rust
//! Fixture I/O: float32 stereo IQ WAV + JSON sidecar (pinned decision 15).

use anyhow::Result;
use num_complex::Complex32;
use std::path::{Path, PathBuf};

pub fn write_fixture(
    dir: &Path,
    name: &str,
    samples: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
) -> Result<PathBuf> {
    let wav_path = dir.join(format!("{name}.wav"));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: fs as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&wav_path, spec)?;
    for s in samples {
        w.write_sample(s.re)?;
        w.write_sample(s.im)?;
    }
    w.finalize()?;

    let sidecar = serde_json::json!({ "center_freq_hz": center_freq_hz });
    std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&sidecar)?)?;
    Ok(wav_path)
}
```

`crates/skimmer-testkit/src/vectors.rs`:

```rust
//! Golden test vectors. SPEC §7: definitions live here
//! (module map §8: "§7 vectors -> skimmer-testkit::vectors").

use crate::scene::{render_scene, SignalSpec};
use crate::wav::write_fixture;
use anyhow::Result;
use num_complex::Complex32;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct VectorSpec {
    pub name: &'static str,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub signals: Vec<SignalSpec>,
}

/// SPEC §7 V1 "clean-20": 20 WPM, +20 dB, offset +12.34 kHz, W1AW,
/// AWGN only, no jitter. M0 = V1 passing end-to-end from a WAV file.
pub fn v1() -> VectorSpec {
    VectorSpec {
        name: "v1",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5631, // "SKIMV1"
        signals: vec![SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
        }],
    }
}

pub struct RenderedVector {
    pub samples: Vec<Complex32>,
    pub keyed_texts: Vec<String>,
    pub expected_freq_hz: f64,
}

pub fn render(spec: &VectorSpec) -> Result<RenderedVector> {
    let (samples, keyed_texts) =
        render_scene(&spec.signals, spec.fs, spec.duration_s, Some(spec.noise_seed))?;
    Ok(RenderedVector {
        samples,
        keyed_texts,
        expected_freq_hz: spec.center_freq_hz + spec.signals[0].offset_hz,
    })
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub name: String,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub expected_freq_hz: f64,
    pub keyed_texts: Vec<String>,
    pub generator: String,
}

/// Write `<name>.wav`, `<name>.json`, `<name>.manifest.json` into `dir`.
pub fn write_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render(spec)?;
    write_fixture(dir, spec.name, &rendered.samples, spec.fs, spec.center_freq_hz)?;
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        keyed_texts: rendered.keyed_texts,
        generator: concat!("skimmer-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-testkit`
Expected: keyer + noise + scene + cer + vectors all PASS. `achieved_snr_matches_request` is the load-bearing one — it validates pinned decision 3 empirically.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(testkit): scene renderer, ref-bw AWGN, CER, V1 golden vector (SPEC §7)"
```

---

### Task 13: `skimmer-engine` — single-channel pipeline

**Files:**
- Create/replace: `crates/skimmer-engine/src/lib.rs`

**Interfaces:**
- Consumes: `skimmer_dsp::{freqest::estimate_peak_hz, single::SingleChannelExtractor}`, `skimmer_decode::{decoder::{TrackDecoder, DecodeConfig, events_to_text}, events::DecoderEvent}`, `skimmer_input::{IqSource, WavIqSource, read_all}`.
- Produces (used by cli):
  - `pub struct PipelineConfig { pub decode: DecodeConfig }` with `Default`
  - `pub struct DecodeReport { pub freq_hz: f64, pub wpm: Option<f32>, pub text: String, pub events: Vec<DecoderEvent> }` (`serde::Serialize`)
  - `pub fn decode_samples(iq: &[Complex32], fs: f64, center_freq_hz: f64, cfg: &PipelineConfig) -> anyhow::Result<DecodeReport>`
  - `pub fn decode_wav(path: &Path, cfg: &PipelineConfig) -> anyhow::Result<DecodeReport>`
- Pipeline: freq estimate (error if none found) → extractor at that offset → per-hop `a = |y|`, `sample_ts = m·hop` → `TrackDecoder` (track_id 1, freq = center + offset) → `finish()` → text + last-reported WPM.

- [ ] **Step 1: Write the failing test** (`crates/skimmer-engine/tests/pipeline.rs`)

```rust
use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::cer::cer;
use skimmer_testkit::scene::{render_scene, SignalSpec};

#[test]
fn v1_lite_decodes_end_to_end() {
    // 20 s slice of the V1 scene: same parameters, faster test. The full
    // 120 s V1 gate lives in skimmer-cli/tests/golden_v1.rs.
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
    };
    let (iq, texts) = render_scene(std::slice::from_ref(&sig), 96_000.0, 20.0, Some(1)).unwrap();
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();
    assert!(
        (report.freq_hz - 14_012_340.0).abs() <= 10.0,
        "freq {} off by {}",
        report.freq_hz,
        (report.freq_hz - 14_012_340.0).abs()
    );
    assert_eq!(cer(&texts[0], &report.text), 0.0, "expected {:?} got {:?}", texts[0], report.text);
    let wpm = report.wpm.expect("wpm reported");
    assert!((wpm - 20.0).abs() < 2.0, "wpm {wpm}");
}

#[test]
fn silence_errors_cleanly() {
    let iq = vec![num_complex::Complex32::new(0.0, 0.0); 96_000];
    // Pure digital silence has no peak; must be an error, not a panic.
    assert!(decode_samples(&iq, 96_000.0, 0.0, &PipelineConfig::default()).is_err());
}
```

(`num-complex` is already a dependency of `skimmer-engine`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p skimmer-engine`
Expected: compile error (`decode_samples` not defined).

- [ ] **Step 3: Implement**

`crates/skimmer-engine/src/lib.rs`:

```rust
//! M0 pipeline: WAV -> frequency estimate -> single channel -> decoder.
//! Grows into the PFB/track-manager engine at M2 (ARCHITECTURE §4, §10).

use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use skimmer_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use skimmer_decode::events::DecoderEvent;
use skimmer_dsp::freqest::estimate_peak_hz;
use skimmer_dsp::single::SingleChannelExtractor;
use skimmer_input::{read_all, IqSource, WavIqSource};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    pub decode: DecodeConfig,
}

#[derive(Debug, serde::Serialize)]
pub struct DecodeReport {
    /// Absolute spot frequency (center + estimated offset), full precision.
    /// SPEC §1.4: full Hz precision belongs to the JSON surface.
    pub freq_hz: f64,
    pub wpm: Option<f32>,
    pub text: String,
    pub events: Vec<DecoderEvent>,
}

pub fn decode_samples(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
) -> Result<DecodeReport> {
    let Some(offset_hz) = estimate_peak_hz(iq, fs) else {
        bail!("no signal found (input shorter than one FFT frame or empty)");
    };
    // Degenerate-input guard: a flat spectrum yields a meaningless argmax.
    // The extractor + demod pre-decode gate will produce no output; that is
    // handled below, but pure digital silence short-circuits here.
    if iq.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
        bail!("input is digital silence");
    }

    let mut extractor = SingleChannelExtractor::new(fs, offset_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channel extractor")?;
    let hop = extractor.hop() as u64;

    let mut decoder = TrackDecoder::new(1, cfg.decode.clone());
    decoder.set_freq_hz(center_freq_hz + offset_hz);

    let channel = extractor.process(iq);
    let mut events: Vec<DecoderEvent> = Vec::new();
    for (m, y) in channel.iter().enumerate() {
        events.extend(decoder.push_envelope(y.norm(), m as u64 * hop));
    }
    events.extend(decoder.finish());

    let wpm = events.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&events);
    Ok(DecodeReport { freq_hz: center_freq_hz + offset_hz, wpm, text, events })
}

pub fn decode_wav(path: &Path, cfg: &PipelineConfig) -> Result<DecodeReport> {
    let mut src = WavIqSource::open(path)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    let iq = read_all(&mut src)?;
    decode_samples(&iq, fs, center, cfg)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p skimmer-engine`
Expected: both tests PASS. This is the first full-chain integration — if `v1_lite_decodes_end_to_end` fails, bisect with the layer tests: freqest alone (Task 9 style), extractor envelope plateau (Task 8 style), then dump `report.events` and compare against the keyed text character by character. Common first failure: the last character missing because `finish()` isn't called — or doubled word boundaries (check `word_flushed` handling).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(engine): M0 single-channel decode pipeline"
```

---

### Task 14: Round-trip proptests (ROADMAP M0 acceptance criterion 2)

**Files:**
- Create: `crates/skimmer-testkit/tests/roundtrip_envelope.rs`
- Create: `crates/skimmer-engine/tests/roundtrip_iq.rs`

**Interfaces:**
- Consumes: everything built so far. No new public API.
- ROADMAP M0: "Proptest round-trip (text → testkit CW → decoder) passes for 10–40 WPM at ≥ +15 dB SNR, CER = 0." The IQ-level test is the criterion; the envelope-level test is a faster, wider-sweep diagnostic that isolates decode bugs from DSP bugs.
- Text strategy notes (both tests): the first character must contain both a dit and a dah (a 2-means tracker cannot disambiguate an all-dah opening with no ratio reference — SPEC §4.1 acknowledges this; real traffic always mixes within a word or two). Texts are 1–3 words of 2–6 chars from `[A-Z0-9]`, first char from the mixed-element set.

- [ ] **Step 1: Write the envelope-level proptest**

`crates/skimmer-testkit/tests/roundtrip_envelope.rs`:

```rust
//! text -> keyed envelope (375 Hz) -> TrackDecoder -> text, CER = 0.
//! Isolates the decode chain from the DSP front end.

use proptest::prelude::*;
use skimmer_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use skimmer_testkit::cer::cer;
use skimmer_testkit::keyer::{key_text, KeyerSpec};

/// First char must contain both elements (see task preamble).
const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn word_strategy(first: bool) -> impl Strategy<Value = String> {
    let head = if first {
        proptest::sample::select(MIXED_FIRST.chars().collect::<Vec<_>>())
    } else {
        proptest::sample::select(REST.chars().collect::<Vec<_>>())
    };
    (head, proptest::collection::vec(proptest::sample::select(REST.chars().collect::<Vec<_>>()), 1..6))
        .prop_map(|(h, tail)| {
            let mut w = h.to_string();
            w.extend(tail);
            w
        })
}

fn text_strategy() -> impl Strategy<Value = String> {
    (word_strategy(true), proptest::collection::vec(word_strategy(false), 0..3))
        .prop_map(|(first, rest)| {
            let mut words = vec![first];
            words.extend(rest);
            words.join(" ")
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn clean_envelope_roundtrip(text in text_strategy(), wpm in 10.0f32..=40.0) {
        let (env, keyed) = key_text(&text, &KeyerSpec::new(wpm), 375.0).unwrap();
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        // Trailing silence: enough for the 7-dit flush AND to guarantee the
        // demod's 375-hop init window fills even for the shortest texts
        // (a 2-char word at 40 WPM is only ~170 hops of envelope).
        let tail = (8.0 * 1200.0 / wpm * 0.375) as usize + 450;
        for i in 0..tail {
            events.extend(dec.push_envelope(0.0, (env.len() + i) as u64 * 256));
        }
        events.extend(dec.finish());
        let decoded = events_to_text(&events);
        prop_assert_eq!(cer(&keyed, &decoded), 0.0, "keyed {:?} decoded {:?}", keyed, decoded);
    }
}
```

Note: a clean envelope has `E_lo` at the 1e-6 floor; the demod's keying-depth gate passes trivially. If init never succeeds, check that Q10 of a 50 %-duty window is 0.0 and the `max(…, 1e-6)` floor is applied (SPEC §3.2).

- [ ] **Step 2: Write the IQ-level proptest**

`crates/skimmer-engine/tests/roundtrip_iq.rs`:

```rust
//! ROADMAP M0 criterion: text -> testkit CW -> (IQ + AWGN) -> full pipeline
//! -> text, CER = 0, for 10–40 WPM at >= +15 dB SNR-in-2500-Hz.

use proptest::prelude::*;
use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::cer::cer;
use skimmer_testkit::keyer::{key_text, KeyerSpec};
use skimmer_testkit::scene::{render_scene, SignalSpec};

const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn word_strategy(first: bool) -> impl Strategy<Value = String> {
    let charset = if first { MIXED_FIRST } else { REST };
    (
        proptest::sample::select(charset.chars().collect::<Vec<_>>()),
        proptest::collection::vec(proptest::sample::select(REST.chars().collect::<Vec<_>>()), 1..6),
    )
        .prop_map(|(h, tail)| {
            let mut w = h.to_string();
            w.extend(tail);
            w
        })
}

fn text_strategy() -> impl Strategy<Value = String> {
    (word_strategy(true), proptest::collection::vec(word_strategy(false), 0..2))
        .prop_map(|(first, rest)| {
            let mut words = vec![first];
            words.extend(rest);
            words.join(" ")
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn iq_roundtrip_with_noise(
        text in text_strategy(),
        wpm in 10.0f32..=40.0,
        snr in 15.0f32..=30.0,
        offset_khz in -40i32..=40,
        noise_seed in any::<u64>(),
    ) {
        let fs = 96_000.0;
        // Scene long enough for the whole text + flush tail.
        let (probe_env, _) = key_text(&text, &KeyerSpec::new(wpm), fs).unwrap();
        let duration_s = probe_env.len() as f64 / fs + 1.5;
        let sig = SignalSpec {
            text: text.clone(),
            loop_text: false,
            wpm,
            offset_hz: offset_khz as f64 * 1000.0,
            snr_2500_db: snr,
            jitter: None,
        };
        let (iq, texts) = render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
        let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
        prop_assert_eq!(
            cer(&texts[0], &report.text), 0.0,
            "wpm {} snr {} offset {} kHz: keyed {:?} decoded {:?}",
            wpm, snr, offset_khz, texts[0], report.text
        );
        // V1's frequency criterion, generalized: within 10 Hz.
        prop_assert!((report.freq_hz - offset_khz as f64 * 1000.0).abs() <= 10.0);
    }
}
```

- [ ] **Step 3: Run both proptests**

Run: `cargo test -p skimmer-testkit --test roundtrip_envelope && cargo test -p skimmer-engine --test roundtrip_iq`
Expected: PASS. **These are the acceptance tests most likely to surface real spec-level bugs.** If a case fails:
1. `proptest` prints the minimal failing input — reproduce it as a standalone `#[test]` before touching code.
2. Diagnose bottom-up: does the envelope-level test pass for the same text/WPM? If yes, the bug is DSP-side (edge softening at high WPM through the 93.75 Hz channel is the known risk); if no, it's decode-side (gap classification at boundary values is the known risk).
3. Fix the code, not the test. If a genuine spec limitation emerges (e.g. 40 WPM edges through the channel filter), STOP and surface it — that's a spec conversation, not a silent test-range narrowing.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "test: envelope- and IQ-level round-trip proptests (ROADMAP M0)"
```

---

### Task 15: `skimmer-cli` — `decode` and `gen` subcommands

**Files:**
- Create/replace: `crates/skimmer-cli/src/main.rs`

**Interfaces:**
- Consumes: `skimmer_engine::{decode_wav, PipelineConfig}`, `skimmer_testkit::vectors`.
- Produces (the M0 user surface):
  - `skimmer decode <fixture.wav>` — stdout: the decoded text (exactly one line). stderr: `freq_hz` and `wpm` diagnostics.
  - `skimmer decode --json <fixture.wav>` — stdout: one JSON object (the full `DecodeReport`, serde_json). This is the byte-comparison surface for the determinism gate.
  - `skimmer gen <vector> --out <dir>` — writes `<vector>.wav` + sidecar + manifest via `write_fixture_set`; errors on unknown vector names (only `v1` at M0).

- [ ] **Step 1: Write the failing integration test** (`crates/skimmer-cli/tests/cli.rs`)

```rust
use std::process::Command;

fn skimmer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_skimmer"))
}

#[test]
fn gen_then_decode_prints_text() {
    let dir = tempfile::tempdir().unwrap();
    // Generate a short fixture through the library (fast), decode via the CLI.
    let spec = skimmer_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..skimmer_testkit::vectors::v1()
    };
    let manifest = skimmer_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = skimmer().arg("decode").arg(dir.path().join("v1.wav")).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim(), manifest.keyed_texts[0]);
}

#[test]
fn gen_subcommand_writes_fixture_set() {
    let dir = tempfile::tempdir().unwrap();
    // NOTE: full 120 s V1 — this is also the fixture-generation smoke test.
    let out = skimmer().args(["gen", "v1", "--out"]).arg(dir.path()).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(dir.path().join("v1.wav").exists());
    assert!(dir.path().join("v1.json").exists());
    assert!(dir.path().join("v1.manifest.json").exists());
}

#[test]
fn unknown_vector_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = skimmer().args(["gen", "v99", "--out"]).arg(dir.path()).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn json_output_is_valid_and_deterministic_across_three_runs() {
    // SPEC §6 CI rule: same binary + same file, 3 runs -> identical output.
    let dir = tempfile::tempdir().unwrap();
    let spec = skimmer_testkit::vectorspec_short();
    let _ = skimmer_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let runs: Vec<Vec<u8>> = (0..3)
        .map(|_| {
            let out = skimmer()
                .args(["decode", "--json"])
                .arg(dir.path().join("v1.wav"))
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        })
        .collect();
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
    let v: serde_json::Value = serde_json::from_slice(&runs[0]).unwrap();
    assert!(v["text"].is_string());
    assert!(v["freq_hz"].is_f64());
    assert!(v["events"].is_array());
}
```

Add the helper to `skimmer-testkit/src/lib.rs` (a 20 s V1 variant used by two test crates):

```rust
/// Short V1 variant for fast integration/determinism tests. Same code path
/// as the full 120 s V1 gate.
pub fn vectorspec_short() -> vectors::VectorSpec {
    vectors::VectorSpec { duration_s: 20.0, ..vectors::v1() }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p skimmer-cli`
Expected: failures — the binary has no subcommands yet.

- [ ] **Step 3: Implement**

`crates/skimmer-cli/src/main.rs`:

```rust
//! `skimmer` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use skimmer_engine::{decode_wav, PipelineConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "skimmer", version, about = "Wideband multi-signal CW skimmer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode a single CW signal from an IQ WAV file (M0 pipeline).
    Decode {
        /// Stereo IQ WAV (ch0 = I, ch1 = Q); center freq from <stem>.json sidecar.
        path: PathBuf,
        /// Emit the full DecodeReport as one JSON object on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Generate a golden test vector fixture set (SPEC §7).
    Gen {
        /// Vector name (M0: "v1").
        vector: String,
        /// Output directory for <name>.wav / .json / .manifest.json.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Decode { path, json } => {
            let report = decode_wav(&path, &PipelineConfig::default())?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
            }
        }
        Command::Gen { vector, out } => {
            let spec = match vector.as_str() {
                "v1" => skimmer_testkit::vectors::v1(),
                other => bail!("unknown vector {other:?} (available: v1)"),
            };
            std::fs::create_dir_all(&out)?;
            let manifest = skimmer_testkit::vectors::write_fixture_set(&spec, &out)?;
            eprintln!(
                "wrote {}/{{{}.wav,{}.json,{}.manifest.json}} (expected freq {:.1} Hz)",
                out.display(), spec.name, spec.name, spec.name, manifest.expected_freq_hz
            );
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p skimmer-cli`
Expected: all 4 PASS (the full-V1 `gen` test takes a few seconds).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add -A && git commit -m "feat(cli): skimmer decode + gen subcommands"
```

---

### Task 16: V1 golden acceptance test, docs, PR finalization

**Files:**
- Create: `crates/skimmer-cli/tests/golden_v1.rs`
- Create: `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`
- Modify: `ROADMAP.md` (one line), `CLAUDE.md` (Status section), `README.md` (quickstart)

- [ ] **Step 1: Write the V1 golden test** (`crates/skimmer-cli/tests/golden_v1.rs`)

This is ROADMAP M0 acceptance criterion 1 / SPEC §7 "M0 = V1 passing end-to-end from a WAV file". It must run in CI on every push (not `#[ignore]`d).

```rust
//! SPEC §7 V1 golden gate: 120 s, 20 WPM, +20 dB, offset +12.34 kHz, W1AW.
//! Pass criteria: char accuracy = 100 %; 1 track; freq error <= 10 Hz.

use std::process::Command;

#[test]
fn v1_passes_end_to_end_from_wav() {
    let dir = tempfile::tempdir().unwrap();
    let spec = skimmer_testkit::vectors::v1();
    let manifest = skimmer_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_skimmer"))
        .args(["decode", "--json"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // char accuracy = 100 %
    let decoded = report["text"].as_str().unwrap();
    assert_eq!(
        skimmer_testkit::cer::cer(&manifest.keyed_texts[0], decoded),
        0.0,
        "V1 char accuracy must be 100 %\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );

    // freq error <= 10 Hz
    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 10.0,
        "freq {} expected {} (err {})",
        freq, manifest.expected_freq_hz, (freq - manifest.expected_freq_hz).abs()
    );

    // 1 track: every event carries track_id 1 (single hardwired channel).
    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }

    // WPM sanity (V1 is 20 WPM; SPEC only gates WPM at V2 but it's free here).
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 20.0).abs() < 2.0, "wpm {wpm}");
}
```

- [ ] **Step 2: Run the golden test**

Run: `cargo test -p skimmer-cli --test golden_v1 -- --nocapture`
Expected: PASS in well under a minute (release-grade opt levels are set in the workspace profile). **If this fails, M0 is not done.** Debug via the engine-level `v1_lite` test first (same scene, 20 s).

- [ ] **Step 3: Write the decisions record**

`docs/DECISIONS/2026-07-11-m0-implementation-pins.md`: copy the "Deviations and pinned decisions" section from this plan verbatim, plus:
- the resolved `<COPPA_REV>` and why (fft-only reuse at M0),
- the actual prototype tap snapshot values (from Task 7 Step 5) and the note that changing them invalidates golden vectors,
- the stopband assertion margin if it had to move (Task 7 Step 4).

- [ ] **Step 4: Doc updates**

- `ROADMAP.md`: change `synthetic 25 WPM / +20 dB SNR / AWGN-only single-signal IQ file` → `synthetic 20 WPM / +20 dB SNR / AWGN-only single-signal IQ file (SPEC §7 V1)` (deviation 1 — ROADMAP already defers to SPEC §7).
- `CLAUDE.md` Status section: `Design phase complete; no implementation yet.` → `M0 implemented (single-signal WAV decode, V1 green); next is M1 in ROADMAP.md.` Add one line under Key constraints: `- M0 testkit generates its own ref-bandwidth AWGN (see docs/DECISIONS/2026-07-11-m0-implementation-pins.md); migrate to coppa awgn_ref_bw when it ships.`
- `README.md`: add a Quickstart: `cargo run -p skimmer-cli -- gen v1 --out /tmp/v1 && cargo run -p skimmer-cli -- decode /tmp/v1/v1.wav`.

- [ ] **Step 5: Full-workspace verification**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: everything green. Fix any clippy debt now, not in review.

- [ ] **Step 6: Commit, push, un-draft**

```bash
git add -A
git commit -m "test: V1 golden acceptance gate; docs: M0 status + decision record"
git push --force-with-lease
```

Then run the superpowers:requesting-code-review skill against the branch, address findings, and mark the PR ready for review (`gh pr ready`). After merge, run /wiki-update to distill: the awgn_ref_bw deviation (extend `coppa-reuse` + `golden-vector-freeze` pages), the demod init-replay pin (new gotcha page candidate), and the M0-shim status of `freqest`/`single` (overview page).

---

## Self-Review (completed at plan-writing time)

**Spec coverage** against ROADMAP M0's four bullets: workspace scaffolding (Task 1); testkit generator (Tasks 11–12); file playback (Task 10); single hardwired channel (Tasks 7–9); classical decoder chain (Tasks 2–6); `skimmer decode fixture.wav` criterion (Tasks 13, 15, 16); proptest criterion (Task 14); CI Linux+macOS without SoapySDR (Task 1, no soapy dep anywhere). SPEC §3–§5 are covered task-by-task; §1.2 by Task 7; §6 rules are embedded in every implementation (no RNG/clock deps exist in dsp/decode/engine — check `Cargo.toml`s in review); §7 V1 by Tasks 12/16. SPEC §1.3 (WOLA PFB), §2 (noise floor/track manager) are deliberately OUT of M0 scope per ROADMAP (M2).

**Known risks accepted:** (1) the 40 WPM end of the IQ proptest exercises keying edges softened by the 93.75 Hz channel — if it fails, that's a real finding to surface, not to paper over; (2) `timing_sigma = 0.25` is flagged by the SPEC itself as its riskiest constant — the proptests are the early-warning system; (3) the stopband property test may sit within ~2 dB of the assertion — margin documented in the decisions file if touched.

**Type consistency:** `Run { mark, start_ts, hops }`, `DecoderEvent` variants, `decode_char(marks, mu_dit, mu_dah, q, cfg) -> Option<CharDecode>`, `SignalSpec` fields, and `write_fixture_set -> Manifest` are used identically across Tasks 5/6/12/13/15/16. `DecodeConfig` has a manual `Default` impl (`flush_gap_dits = 7.0`; a derived default would zero it). `vectorspec_short()` added in Task 15 is used by Task 15's determinism test.
