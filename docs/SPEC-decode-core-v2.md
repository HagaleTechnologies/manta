# SPEC — Decode Core v2 (evidence front end + HSMM sequence decoder)

Status: **draft v1, proposed** (2026-09-09, MAN-166). Companion to
`docs/superpowers/specs/2026-09-09-decode-core-real-hf-design.md` (the
rationale and evidence) and `docs/SPEC-decode-core.md` (v1). Nothing here
is implemented.

Relationship to v1: §1 (channelizer), §2.1–§2.5 (detection floor, gate,
track lifecycle, ownership), §5 (events), §6 (determinism), §7 (vectors)
and §9 (config) of `SPEC-decode-core.md` **stay in force**. This document
**supersedes v1 §3 (per-track demodulation) and §4 (element classification
and Morse decoding) whenever `decode.engine = "hsmm"`**, and **extends**
v1 §2.1 with a per-track noise reference (§2 below) and v1 §5 with
additive event fields (§5 below). With `decode.engine = "legacy"` the v1
chain runs unchanged.

Notation as v1: `fo = 375 Hz`, hop = 2.667 ms, `t` = hop index, `k` =
channel index. All millisecond constants convert to hops with v1 §1.1's
single rounding rule. "Dit unit" `u` is a per-token real number in hops;
`WPM = 450 / u` (PARIS: 1200 ms / (u · 8/3 ms)).

---

## 1. Evidence front end (`manta-decode::evidence`)

Input per hop: `a[t] = sqrt(P[t])`, the linear amplitude of the track's
channel sample (v1 §3 input; or the refined magnitude of §3 when enabled),
and the track-local noise power `N[t]` from §2.

### 1.1 Smoothing

`a_s[t]` = EMA of `a[t]` with `τ_a = 8 ms` (`α_a = 1 − e^{−2.667/8} =
0.283`). Purpose: a single-hop impulse must not set the mark level.

### 1.2 Mark level (centered maximum)

`M[t] = max(a_s[t−h … t+h])`, `h = round(hold_dits · u_ref)` hops, where
`u_ref` is the best live token's `u` (§4.5) or `u_init = 15` hops
(40 ms) before any token exists. Default `hold_dits = 4`. Implemented as a
sliding-window maximum (monotonic deque, O(1) amortized). Because the
window is centered, the evidence stream is produced with a fixed delay of
`h` hops behind the input; all timestamps refer to the input hop.

Rationale (design §1.3, §5.1): a *symmetric* window tracks QSB in both
directions with no rise/decay asymmetry to tune, and a maximum (not a
mean) cannot be dragged by a long dah or a run of dits.

### 1.3 Space level

`n[t] = sqrt(N[t])`, `N[t]` from §2.

### 1.4 Keying-present gate

`present[t] = (M[t] ≥ 2 · n[t])` (6 dB apparent keying depth, the same
bar as v1 §3.2's pre-decode state). While `present[t]` is false:
`llr[t] = −L` (§1.6) and no tokens are seeded (§4.7).

### 1.5 Normalized amplitude

`u_amp[t] = clamp((a[t] − n[t]) / max(M[t] − n[t], ε), −0.5, 1.5)`,
`ε = 1e-9`. Note the *raw* `a[t]` is normalized, not `a_s`; only the
level tracker is smoothed.

### 1.6 Per-hop log-likelihood ratio

Mark and space are modeled as equal-variance Gaussians in `u_amp`
centered at 1 and 0, `σ_u` default 0.30:

`llr[t] = clamp((u_amp[t] − 0.5) / σ_u², −L, +L)`, `L` default 10.

Properties the implementation must preserve: `llr` is antisymmetric
about `u_amp = 0.5` (the half-amplitude crossing is the decision surface
at every keying depth — design §1.3), and its sum over any interval is a
matched filter for a rectangular mark of that length.

### 1.7 Anchors (edge triggers)

Hop `t` is an **anchor** iff `sign(llr[t]) ≠ sign(llr[t−1])` (a
half-amplitude crossing) or `t mod fallback_hops = 0` (default 8), or
`present` changed at `t`. The decoder (§4) evaluates segment ends only at
anchors and segment starts only at earlier anchors; segment durations are
therefore exact anchor-to-anchor distances, and no duration grid is
needed.

### 1.8 Prefix sums

`C[t] = Σ_{i≤t} llr[i]`, accumulated in `f64` sequentially. The mark
evidence of a segment `(s, t]` is `C[t] − C[s]`; space evidence is its
negation.

---

## 2. Track-local noise reference (`manta-dsp::floor`, additive)

v1 §2.1's 25th-percentile-over-10 s floor remains the *detection* floor.
For the decoder, each track computes per hop:

### 2.1 Temporal minimum statistics

`P_s[k, t]` = EMA of channel power `P[k, t]` with `τ = 40 ms` (v1 §2.3's
gate EMA may be reused). `N_temp[t] = B_min · min(P_s[k, t−W … t])`,
`W = round(noise_window_ms / 2.667)` hops, default `noise_window_ms =
1500`. `B_min` is the bias correction for the minimum of the smoothed
chi-square(2) power over the window; default `noise_min_bias_db = 2.5`
(`B_min = 10^{0.25}`). Sliding minimum via monotonic deque.

### 2.2 Spectral reference (guard-banded neighbors)

With `c = round(track centroid)` (v1 §2.5): `N_spec[t] = B_spec ·
min(P_s[k, t] for k ∈ {c±2, c±3, c±4})`, six cells with a one-channel
guard on each side. Default `spectral_min_bias_db = 1.5`.
`FloorBank` exposes `fn spectral_reference_db(&self, c: usize) -> f64`
computing this from its existing per-channel smoothed powers (the gate
EMA), so no new per-channel state is needed.

### 2.3 Combination

`N[t] = max(N_temp[t], β · N_spec[t])`, `β` = `spectral_beta`, default
0.5. Rationale: a neighbor's keying burst or click that leaks into `k`
also leaks into `k±2…4`, so the reference rises with it and §1.6
discounts exactly those hops; a clean CW neighbor contributes only when
it is the minimum of six cells.

### 2.4 Calibration test

On a noise-only channelizer output (testkit AWGN, no signal), the mean
of `N_temp[t]` over 60 s must be within ±0.5 dB of the true mean channel
power; same for `N_spec` on a 7-channel noise-only block. These tests
pin `B_min` and `B_spec`.

---

## 3. Per-track narrowband refinement (`manta-dsp::refine`, optional)

Enabled when `decode.refine_bw_hz > 0` (default 0 = off). Per hop, for a
track with fractional centroid `c_f` (v1 §1.4) and nearest channel
`c = round(c_f)`, `δ = c_f − c` channels:

1. `y[t] = X[c, t] · e^{−j 2π δ Δ t / fo}` (phase accumulator in `f64`,
   wrapped each hop; `Δ = 93.75 Hz`).
2. `z[t] = Σ_{i=0}^{10} g[i] · y[t−i]`, `g` = 11-tap windowed-sinc
   lowpass at `fo` with cutoff `refine_bw_hz` (Hamming window), unity DC
   gain, designed once at startup in `f64`, stored `f32`.
3. `a[t] = |z[t]|` replaces v1 §3's `sqrt(max-power owned channel)` as
   §1's input.

Recommended `refine_bw_hz = 30` (10–90 % step rise ≈ 12 ms; at 45 WPM a
dit is 27 ms). The channel selection `c` follows the centroid; a change
of `c` resets the FIR history (one lost hop of evidence, `llr = 0`).
The call site is `manta-engine`'s per-hop track feed (one line); this
section specifies the module, not the engine change.

---

## 4. HSMM token-passing decoder (`manta-decode::hsmm`)

### 4.1 Grammar

The Morse tree of v1 §4.4 (root, dit = left child, dah = right child,
glyphs and prosigns on nodes; max depth 7). `ROOT` has no glyph.

### 4.2 Segment types

| type | phase it ends | nominal length | after |
|---|---|---|---|
| `Dit` | mark | `1·u` | node → dit-child, phase = AfterMark |
| `Dah` | mark | `3·u` | node → dah-child, phase = AfterMark |
| `EGap` | space | `1·u` | node unchanged, phase = AfterSpace |
| `CGap` | space | `3·u` | emit glyph(node), node = ROOT, AfterSpace |
| `WGap` | space | `7·u` | emit glyph(node) + word boundary, node = ROOT |
| `Silence` | space | ≥ `10·u` | as `WGap`; flat duration prior |

`CGap`/`WGap`/`Silence` are only allowed from a glyph-bearing node. A
mark segment is only allowed if the child exists. A mark whose observed
`d > 1.5·3·u` or a space with `d > 1.5·7·u` and `< 10·u` has no valid
type (the token dies).

### 4.3 Duration prior

For type with nominal `k·u` and observed `d` hops:
`lp_dur = −(ln(d / (k·u)))² / (2·σ_d²)`, `σ_d` = `dur_sigma`, default
0.22, evaluated only for `0.6·k·u ≤ d ≤ 1.5·k·u`. `Silence`:
`lp_dur = 0` for `10·u ≤ d ≤ 80·u`.

### 4.4 Type prior

`lp_type`: `Dit` `ln 0.6 + π_ins`, `Dah` `ln 0.4 + π_ins`, `EGap` `ln 0.62`,
`CGap` `ln 0.28`, `WGap` `ln 0.10`, `Silence` `ln 0.10 − 4`. `π_ins` =
`mark_insert_penalty`, default −1.5 (prevents noise blips from being
explained as `E`).

### 4.5 Token

```
Token { node: NodeId, phase: AfterMark | AfterSpace, u: f32,
        score: f32, hist: RingBuf<Entry, 8>, anchor: u64 }
Entry = Glyph(Glyph) | Word
```

`u` is updated after every `Dit`/`Dah`/`EGap`/`CGap` of observed `d`:
`u ← u + α_u · (d/k − u)`, `α_u` = `speed_alpha`, default 0.2, then
clamped to `[7.5, 56]` hops (60…8 WPM). `WGap`/`Silence` do not update
`u`.

### 4.6 Per-anchor step

At each anchor `t` (ascending), with `A(t)` the set of earlier anchors
`s` with `t − s ≤ 1.5 · 7 · u_max_live` (and up to `80·u` for `Silence`,
sampled every `round(u)` hops):

1. For each `s ∈ A(t)` ascending, for each token `τ` alive at `s` in
   ascending stored order, for each segment type valid for `τ.phase`,
   `τ.node`, and `d = t − s`: `score' = τ.score + ev + lp_dur + lp_type`
   where `ev = C[t] − C[s]` for marks and `−(C[t] − C[s])` for spaces.
   Build the successor token per §4.2 (copy `hist`, append `Entry` on
   `CGap`/`WGap`/`Silence`, update `u` per §4.5, `anchor = t`).
2. **Merge**: successors with equal `(node, phase, round(2·u))` keep the
   one with the higher `score`; ties by §6.
3. **Prune**: keep the best `beam` (default 12) by `score` (ties by §6).
4. **Commit** (§4.8).
5. Tokens at anchors older than `t − 80·u_max_live` are discarded.

### 4.7 Seeding

When `present` rises (§1.4), or after any committed `Silence`, seed
tokens `{node: ROOT, phase: AfterSpace, u ∈ seed_units_hops, score: 0,
hist: empty, anchor: t}` for `seed_units_hops` default
`[9, 13, 18, 26, 38]` (≈ 50, 35, 25, 17, 12 WPM). Seeds compete with any
live tokens on equal terms; the first character's evidence prunes them.
This replaces v1 §4.1's 5-mark initialization, its unconfirmed/placeholder
states, the ratio clamp, and the drift/regime rule.

### 4.8 Commit

After pruning at anchor `t`:

- **Consensus**: let `p` be the longest common prefix of `hist` across
  all live tokens. Emit each entry of `p` in order (§5) and remove it
  from every token's `hist`.
- **Forced**: if the best token's oldest `hist` entry was appended more
  than `lookahead_dits · u_best` hops ago (default 25), emit it, remove
  it from tokens that agree, and drop tokens that disagree.
- A `Glyph` entry emits `CharDecoded { sample_ts = start of the closing
  space segment, glyph, confidence, alternatives }`; a `Word` entry emits
  `WordBoundary { sample_ts = start of the word gap, confidence }` (v1
  pinned decision 11's timestamp convention is kept).
- `SpeedUpdate` is emitted when `450 / u_best` moves ≥ 1 WPM from the
  last report (v1 §5).

### 4.9 Confidence and alternatives

At emission of entry `j` from the best token (score `s_best`):
`s_alt` = the highest score among live tokens whose entry `j` differs
(or `s_best − 4κ` if none), `κ` = `conf_kappa`, default 6.
`confidence = σ((s_best − s_alt)/κ) · q`, `σ` the logistic function,
`q = clamp(SNR_2500 / 20, 0.3, 1.0)` exactly as v1 §4.5. `alternatives`
= up to 2 distinct competing glyphs at position `j`, each with
`σ((s_i − s_best)/κ)`, ordered by score then §6. A `WordBoundary`'s
confidence uses the same margin between the `WGap` interpretation and
the best token whose entry `j` is not `Word`.

### 4.10 End of stream

`finish()` performs a forced commit of the best token's whole `hist`
and, if its phase is `AfterMark` at a glyph-bearing node, emits that
glyph with confidence `σ(0)·q` (the last character is not closed by a
gap).

---

## 5. Events (v1 §5, additive)

```
CharDecoded  { track_id, sample_ts, glyph, confidence,
               #[serde(default)] alternatives: Vec<(Glyph, f32)> }
WordBoundary { track_id, sample_ts, #[serde(default)] confidence: f32 }
```

Defaults (`[]`, `1.0`) keep every existing consumer and the JSON report
byte-compatible when the fields are absent. The legacy engine emits the
defaults. The spot wire schema (`dispensa`) is untouched.

---

## 6. Determinism (v1 §6, additions)

1. Anchors are processed in ascending hop order; `A(t)` ascending;
   tokens in stored order; segment types in the order of the §4.2 table.
2. Ordering key for merge/prune ties: `score` desc (`total_cmp`), then
   `hist` lexical (glyph code-point order, `Word` < any glyph), then `u`
   asc, then `node` asc, then `phase` (`AfterSpace` < `AfterMark`).
3. `C[t]` in `f64`; `score`, `u`, `llr` in `f32` with fixed operation
   order; no FMA-dependent reductions.
4. No RNG, no wall clock; `M[t]` and `N[t]` windows are hop-counted.
5. Output is a pure function of the track's input; pool order cannot
   affect it (v1 §6.6).

CI: same binary + same IQ file, 3 runs → identical SHA-256 of the JSON
report, with `decode.engine = "hsmm"`.

---

## 7. Configuration keys (v1 §9, additive)

```toml
[decode]
engine = "legacy"           # "legacy" | "hsmm"
# §1
sigma_u = 0.30              llr_clip = 10.0          hold_dits = 4
fallback_hops = 8
# §2
noise_window_ms = 1500      noise_min_bias_db = 2.5
spectral_min_bias_db = 1.5  spectral_beta = 0.5
# §3
refine_bw_hz = 0
# §4
dur_sigma = 0.22            mark_insert_penalty = -1.5
beam = 12                   lookahead_dits = 25      speed_alpha = 0.2
seed_units_hops = [9, 13, 18, 26, 38]
conf_kappa = 6.0
```

Every key is exposed exactly as v1 §9's are (TOML + `DecodeConfig`).

---

## 8. Test vectors

### 8.1 Existing

V1–V10 (v1 §7) must pass with `engine = "hsmm"` at their stated bars,
**including V2, V5, V6, V8w** (the open fading/speed limitations are
inside this engine's design goals, not deferred).

### 8.2 New "real-conditions" vectors (VR), generated by `manta-testkit`

All: `fs = 96 000`, 120 s, seeds recorded, SNR in 2500 Hz, text
`CQ TEST <CALL> <CALL> TEST` unless stated.

| # | name | signal | impairment | pass criteria |
|---|---|---|---|---|
| VR1 | contest-40 | 40 WPM, +30 dB, W5AU | AWGN, jitter 8 % | char ≥ 95 %; WPM 40 ± 2; word boundaries ≥ 95 % |
| VR2 | contest-45 | 45 WPM, +30 dB, K5TR | AWGN, jitter 8 % | char ≥ 90 %; WPM 45 ± 3 |
| VR3 | deep-keying | 30 WPM, +45 dB (≈ 60 dB in channel), N5ZZ | AWGN | char ≥ 98 %; measured dit within ±10 % of 40 ms |
| VR4 | hard-edges | 32 WPM, +25 dB, rise 0.5 ms (click sidebands), G3ABC | AWGN | char ≥ 90 % |
| VR5 | co-channel | A: 30 WPM +25 dB at +10.000 kHz; B: 24 WPM +15 dB at +10.030 kHz (same channel) | AWGN | A: char ≥ 90 %; 0 bogus callsigns |
| VR6 | weighting | 28 WPM, +20 dB, dah:dit 2.6 and 3.4 (two scenes), F6XYZ | AWGN | char ≥ 95 % both |
| VR7 | tight-spacing | 35 WPM, +20 dB, word gaps 5 dits, char gaps 2.5 dits, JA1ZZZ | AWGN | word boundaries ≥ 90 %; char ≥ 95 % |
| VR8 | qsb-fast | 30 WPM, +10 dB, Watterson CCIR-poor, DL9ABC | Watterson | char ≥ 85 % |

`manta-testkit::keyer` gains `rise_ms` per scene (exists), `weight`
(dah/dit ratio), and `gap_units` (character/word gap multipliers) to
render VR6/VR7; `scene` gains a co-channel pair for VR5.

### 8.3 Real-signal oracle (`manta-testkit::oracle`)

Inputs: an IQ WAV + sidecar, the RBN daily-dump CSV, a spotter filter
(default `K5TR` for B2), a window length (default 40 s). For each
(call, kHz) spot: channelize with `manta_dsp::Channelizer`, take the
strongest of the three channels around the kHz, feed the power stream to
`TrackDecoder` with the configured engine, and report per spot and in
aggregate:

- `as_word`: callsign appears as a whole word in the decoded text;
- `framed`: `CQ|DE|TEST [TEST] <call>` appears;
- `substring`: callsign appears with whitespace removed;
- `wpm_ratio`: last `SpeedUpdate` ÷ RBN speed;
- buckets by RBN SNR (< 15, 15–25, ≥ 25 dB).

Output: JSON Lines per spot plus a summary line. Runtime target: the 221
B2/K5TR windows in ≤ 2 min on a laptop. Baseline to beat (legacy engine,
2026-09-09): `as_word 27 %`, `framed 13 %`, `substring 48 %`,
`wpm_ratio 0.82`.

### 8.4 Gates (design §6)

| stage | gate |
|---|---|
| 1 (front end feeding the legacy chain via a −6 dB decision) | oracle `as_word ≥ 45 %`, `wpm_ratio ∈ [0.95, 1.05]`; V1–V10 no regression; `CHAR_GAP_DITS` back to 2.0 with V2 passing |
| 2 (hsmm) | oracle `as_word ≥ 60 %`, `framed ≥ 40 %`; V1–V10 + VR1–VR8 pass; determinism CI; criterion bench ≤ 25 % of one M-series core for 300 tracks at 192 kS/s including the PFB |
| 3 (lattice) | MAN-106 separation: median confidence of true vs bogus spots differs by ≥ 0.3 on the V8w scene and on B2/K5TR |

---

## 9. Module map

| section | crate::module | replaces / extends |
|---|---|---|
| §1 | `manta-decode::evidence` | v1 §3 `envelope::Demod` (kept for `engine = "legacy"`) |
| §2 | `manta-dsp::floor` (`spectral_reference_db`) + `manta-decode::noise` | extends v1 §2.1 |
| §3 | `manta-dsp::refine` | new; call site in `manta-engine` track feed |
| §4 | `manta-decode::hsmm` (`tokens.rs`, `segments.rs`, `commit.rs`) | v1 §4 `timing`, `beam` (kept for legacy) |
| §4 glue | `manta-decode::decoder::TrackDecoder` gains an `Engine` enum | — |
| §5 | `manta-decode::events` | additive |
| §8.2 | `manta-testkit::vectors`, `keyer`, `scene` | additive |
| §8.3 | `manta-testkit::oracle` + `crates/manta-cli` subcommand `oracle` | new |

## 10. CPU budget (design §5.6)

Per anchor: `|A(t)| ≤ ~12` × `beam 12` × ≤ 3 valid types ≈ 430
candidate evaluations of ~10 flops. Anchors ≈ 2 per element + fallback
≈ 100/s per track at 35 WPM → ≈ 0.4 MFLOP/s per track, **≈ 130 MFLOP/s
worst case for 300 simultaneously keying tracks**, typically a third of
that (most tracks are idle or slow). §1–§2 add < 40 op per hop per track
(≈ 4.5 MFLOP/s for 300 tracks). Criterion bench `hsmm_300_tracks` gates
this; `beam` and `fallback_hops` are the knobs if the Pi 4 leg needs
them.
