# SPEC — Decode Core (channelizer → detector → demod → Morse decode)

Status: draft v1. Companion to `ARCHITECTURE.md` §4–§5. This document is the
implementation-level specification: an implementer should be able to code
`manta-dsp` and `manta-decode` from this file without making design
decisions. All constants are normative defaults; every one is exposed in the
TOML config under the names given in [§9](#9-configuration-keys).

Deviations from ARCHITECTURE.md are marked **[DEVIATION]** and summarized in
[§10](#10-deviations-from-architecturemd).

Notation: `fs` = input complex sample rate (S/s), `N` = FFT/channel count,
`Δ` = channel spacing (Hz), `m` = hop index, `k` = channel index `0..N-1`,
`hop` = samples advanced per PFB output frame.

---

## 1. Channelizer (4×-oversampled polyphase filterbank)

### 1.1 Dimensions

`N = fs / 93.75`, which is a power of two for all supported rates:

| Input rate `fs` | `N` | Spacing `Δ = fs/N` | `hop = N/4` | Output rate `fo = fs/hop` | Hop period |
|---|---|---|---|---|---|
| 96 000  | 1024 | 93.75 Hz | 256  | 375 Hz | 2.667 ms |
| 192 000 | 2048 | 93.75 Hz | 512  | 375 Hz | 2.667 ms |
| 384 000 | 4096 | 93.75 Hz | 1024 | 375 Hz | 2.667 ms |
| 768 000 | 8192 | 93.75 Hz | 2048 | 375 Hz | 2.667 ms |

Non-power-of-two input rates (e.g. KiwiSDR 12 kHz IQ) are rational-resampled
in `manta-input` to the nearest table rate before the channelizer;
the channelizer itself only ever sees a table rate. Audio-passband mode
(48 kHz real) is Hilbert-transformed then treated as `fs = 48 000`, `N = 512`
(same 93.75 Hz spacing; the table generalizes as `N = fs/93.75`).

All downstream timing constants are defined in milliseconds and converted to
hops at `fo = 375 Hz` exactly once, at startup, rounding half-up to the
nearest integer hop (the single normative rounding rule for all ms→hop
conversions in this spec). `fo` is invariant across input
rates by construction; nothing below depends on `fs`.

### 1.2 Prototype filter

**[DEVIATION]** `coppa-dsp::filter` provides only `RrcFilter` (root-raised
cosine); there is no general Kaiser/windowed-sinc designer to reuse. The
prototype designer is therefore new code in `manta-dsp::proto` (~40 lines:
Kaiser window + windowed sinc). `coppa-dsp::fft::FftProcessor` is reused as
specified.

Prototype lowpass, length `L·N` with **L = 8 taps per branch**:

- Windowed-sinc: `h[n] = sinc(2·f_c·(n − (LN−1)/2) / fs) · w_kaiser[n]`,
  `n = 0..LN-1`, normalized so `Σ h[n] = 1` (unity DC gain per channel).
- Cutoff `f_c = Δ/2 = 46.875 Hz` (−6 dB point at the channel edge; adjacent
  channels cross at −6 dB, giving the required ≥50 % spectral overlap so a
  signal straddling an edge loses ≤ 3 dB in the better channel after the §1.4
  interpolation).
- Kaiser `β = 7.857` (target stopband attenuation **A = 80 dB**;
  `β = 0.1102·(A − 8.7)`). With `LN ≥ 8192` taps at 96 kS/s the transition
  band is ≈ 60 Hz — stopband is reached by ~107 Hz offset, i.e. alias
  rejection ≥ 80 dB from 1.15 channels away.
- Coefficients are computed in `f64` and stored as `f32`, generated once at
  startup; the same function is unit-tested against fixed reference values
  (first/middle/last 4 taps at N=1024 pinned to 1e-7).

### 1.3 Structure and per-hop processing (WOLA form)

Maintain a sliding input window `x[0..LN)` (newest sample at the end). Every
`hop` new samples:

1. **Window & fold:** `u[n] = x[n] · h[LN−1−n]` for `n = 0..LN`, then fold to
   `N` points: `v[j] = Σ_{p=0..L-1} u[j + pN]`, `j = 0..N`.
2. **Phase correction for hop < N:** circularly rotate `v` left by
   `r = (m · hop) mod N` samples: `v'[j] = v[(j + r) mod N]`. (Equivalent to
   multiplying bin `k` by `e^{+j2πk·m·hop/N}`; the rotation keeps every
   channel's passband centered at DC in its own output stream so the envelope
   is phase-continuous across hops.)
3. **FFT:** `X[k, m] = FFT_N(v')[k]` via `FftProcessor::new(N)` (one instance,
   created once; `forward()` allocates — acceptable at 375 calls/s, but the
   engine may pre-allocate via `try_forward` into a scratch buffer if profiling
   demands; not required for M0/M1).
4. **Power:** `P[k, m] = |X[k, m]|²`. Report in dB: `PdB = 10·log10(P + ε)`,
   `ε = 1e-20`.

Channel `k` corresponds to RF frequency
`f(k) = f_center + ((k + N/2) mod N − N/2) · Δ` (standard FFT bin order,
negative frequencies in the upper half). All detector/decoder code works in
channel index; conversion to Hz happens only when a spot is emitted.

### 1.4 Fine frequency estimate (for ±10 Hz spot accuracy)

Per hop, for a track with peak channel `k₀`:

- Quadratic interpolation on **dB** powers of `(k₀−1, k₀, k₀+1)`:
  `δ_m = 0.5·(P₋ − P₊) / (P₋ − 2P₀ + P₊)` where `P• = PdB[k₀•, m]`.
  Clamp `δ_m` to `[−0.5, +0.5]`; if the denominator ≥ 0 (no local max — a
  peak requires `P₋ − 2P₀ + P₊ < 0`), set `δ_m = 0` and mark the hop unusable.
- Only **key-down** hops (§3.4) with `SNR ≥ 6 dB` contribute.
- Track centroid: power-weighted running mean over the track lifetime:
  `C = Σ (k₀ + δ_m)·P₀[m] / Σ P₀[m]` (accumulate in `f64`).
- Spot frequency: `f_spot = f(0) + C·Δ` rounded to 0.1 kHz for the telnet
  output, full precision (Hz) in the JSON stream.

With ≥ 100 key-down hops (any real CW transmission) the estimator's standard
error is ≪ 10 Hz; absolute accuracy is then bounded by the SDR's reference
oscillator, which is out of scope (config `input.freq_correction_ppm` exists).

---

## 2. Noise floor & signal-presence detection

### 2.1 Per-channel floor estimator

Order-statistic estimator over a sliding window, computed from a decimated
power stream to bound cost:

- Every 15th hop (25 Hz), push `PdB[k, m]` into a per-channel ring of
  **250 entries (10 s)**.
- Maintain a per-channel histogram of the ring contents: 0.5 dB bins spanning
  −140..0 dBFS (280 bins, `u8` counts; increment on push, decrement on evict —
  O(1) per update, no sorting).
- Floor `F_ch[k]` = the **25th percentile** of the histogram. Median is NOT
  used: CW key-down duty cycle reaches 50–60 % on a busy channel, which
  inflates the median by the full signal power; the lower quartile stays on
  the noise rail unless duty exceeds 75 %.

### 2.2 Neighborhood floor and effective floor

A channel occupied continuously for > 10 s still inflates its own quartile.
Guard with a spectral-neighborhood floor:

- Group channels into blocks of 32. `F_blk[b]` = median of the 32 `F_ch`
  values in block `b`, recomputed at 25 Hz.
- **Effective floor:** `F[k] = min(F_ch[k], F_blk[⌊k/32⌋] + 3 dB)`.

The +3 dB allowance tolerates genuine floor slope across a block (e.g. filter
edges); the `min` guarantees a parked carrier can never raise its own
detection threshold by more than 3 dB relative to its neighbors.

Startup: for the first 10 s the ring is partially filled; the quantile is
taken over whatever is present, and track creation is inhibited for the first
**2 s** (`detector.warmup_ms = 2000`) to avoid floor-transient garbage.

### 2.3 Gate

Per channel, smoothed power `S[k, m]`: EMA of `PdB[k, m]` with time constant
**τ = 40 ms** (`α = 1 − e^{−2.667/40} = 0.0645`).

- **Rise:** `S ≥ F + 6 dB` (`detector.on_snr_db = 6.0`) sustained for
  **19 consecutive hops (≈ 50 ms)** — rejects impulse noise and clicks.
- **Drop:** `S < F + 3 dB` (`detector.off_snr_db = 3.0`, i.e. 3 dB
  hysteresis) continuously for **hang = 5 000 ms** (1875 hops) — survives QSB
  troughs and inter-word gaps at slow speeds.

Reported track SNR (for spots) is converted from the 93.75 Hz channel to the
conventional 2500 Hz reference bandwidth:
`SNR_2500 = (S − F) − 10·log10(2500/93.75) = (S − F) − 14.3 dB`.

### 2.4 Track lifecycle state machine

States: `IDLE → CANDIDATE → ACTIVE → HANG → CLOSED`.

| Transition | Condition |
|---|---|
| IDLE → CANDIDATE | rise condition first met on channel `k`, and `k` is not owned by an existing track (§2.5) |
| CANDIDATE → ACTIVE | rise sustained 19 hops → lease decoder from pool |
| CANDIDATE → IDLE | rise condition lost before 19 hops |
| ACTIVE → HANG | drop condition met (below off threshold) |
| HANG → ACTIVE | `S ≥ F + on_snr_db` again (hang timer reset) |
| HANG → CLOSED | hang timer (5 000 ms) expires → decoder returned, final spots flushed |
| ACTIVE/HANG → CLOSED | **garbage collect:** no character emitted for 30 000 ms (`detector.gc_ms`) — carrier or non-CW signal; the channel is marked *suppressed* for 60 s (re-detection allowed but logged) |
| any → CLOSED | eviction: track cap reached and this is the lowest-SNR track (counted in metrics, per ARCHITECTURE §4) |

All timers are hop-counted (integers), never wall-clock.

### 2.5 Adjacent-channel ownership (one signal ⇒ one track)

- A track tracks a fractional center `c` (the §1.4 running centroid,
  initialized to its birth channel `k₀`). It **owns** channels
  `{round(c) − 1, round(c), round(c) + 1}`.
- A CANDIDATE in an owned channel is absorbed: no new track; ownership stays
  with the incumbent.
- Each hop, the track's demod input is taken from the **max-power channel
  among its owned set** (per ARCHITECTURE §4 "max-power selection"); the
  owned set follows `round(c)` as the centroid drifts, which is how drifting
  signals (§7 test 9) are followed without retuning.
- If two channels `k` and `k+1` meet the rise condition on the *same hop* and
  neither is owned, one CANDIDATE is created at the higher-power channel.
- Two tracks whose centers converge within 1.0 channel (interference or
  drift-collision) are merged: the lower-SNR track is CLOSED with reason
  `merged` (counted); its decoder state is discarded (text already emitted
  stands).

---

## 3. Per-track demodulation

Input: `a[m] = sqrt(max-power-owned-channel P)` — the linear envelope at
375 Hz. All constants below in ms are converted to hops (2.667 ms/hop).

### 3.1 Normalization

**[DEVIATION — narrowed]** `coppa-dsp::agc::AdaptiveAgc` is block-based
(`new(target_level, block_size)`, `process(&[f32])`) and designed for audio
block flows. At 375 Hz a meaningful block is 16 samples ≈ 43 ms of latency
and its adaptation interacts with the keying envelope itself. Since the §3.2
threshold is self-normalizing (it estimates both rails), AGC adds no decision
value. **Normalization is a single fixed scale per track:** divide `a[m]` by
`A_ref` = the 90th percentile of `a` over the first 500 ms after ACTIVE
(re-estimated once if `E_hi` later drifts above `3·A_ref` or below
`A_ref/3`). `AdaptiveAgc` is not used in the decode path.

### 3.2 Dual-EMA adaptive keying threshold

State: `E_hi` (key-down level), `E_lo` (key-up level), threshold
`T = sqrt(E_hi · E_lo)` (geometric mean, per ARCHITECTURE §5).

Initialization, from the first 375 hops (1 s) after ACTIVE:
`E_hi = Q90(a)`, `E_lo = max(Q10(a), 1e-6)`. If `E_hi / E_lo < 2` (< 6 dB
apparent keying depth) the track stays in a *pre-decode* state and
re-attempts initialization every 1 s; no elements are emitted (prevents
decoding carriers/noise).

Per-hop update — update only the rail the sample belongs to:

```
if a[m] > T:  E_hi ← E_hi + α_hi · (a[m] − E_hi)
else:         E_lo ← E_lo + α_lo · (a[m] − E_lo)
T = sqrt(E_hi · E_lo)
```

Time constants: `τ_lo = 500 ms` fixed
(`α_lo = 1 − e^{−2.667/500} = 0.00532`).
`τ_hi` is WPM-adaptive once speed is tracked (§4.1):
`τ_hi = clamp(5 · dit_ms, 100, 400) ms`, initial 200 ms. Rationale: `E_hi`
must ride QSB (fast) but average over several elements (≥ 5 dits) so a single
stretched dah doesn't drag it.

Floors: `E_hi ≥ 2·E_lo` is enforced after every update (if violated, set
`E_hi = 2·E_lo`); prevents rail collapse during long silences.

### 3.3 Key decision with hysteresis and debounce

- Key-down when `a[m] > 1.25·T`; key-up when `a[m] < 0.80·T`
  (±1.9 dB hysteresis about `T`); between the two bounds the previous state
  holds.
- **Debounce:** a run (mark or space) shorter than **12 ms (≈ 4.5 hops)** is
  merged into its neighbors (the two adjacent runs and the short run become
  one run of the neighbors' polarity). 12 ms ≈ half a dit at 50 WPM — nothing
  legitimate is that short.

### 3.4 Element stream

Output of this stage: alternating `Mark(duration_hops)` /
`Space(duration_hops)` events, timestamped by the sample counter of their
leading edge. A space is not emitted until the next mark begins (open-ended
trailing space is flushed as a word boundary by the 7-dit timeout rule,
§4.2).

---

## 4. Element classification & Morse decoding

### 4.1 Online 2-means speed tracking (marks)

State: `μ_dit`, `μ_dah` in ms. Boundary between clusters:
`B = sqrt(μ_dit · μ_dah)` (geometric mean ≈ `1.73·μ_dit` at nominal 3:1).

**Initialization** — after the first **5 marks**: sort durations;
find the largest ratio gap between consecutive sorted values. If
`max/min ≥ 2.0`, split at the largest gap: `μ_dit` = mean below,
`μ_dah` = mean above. Otherwise (all one cluster — e.g. `EEE` or `TTT`):
`μ_dit = mean`, `μ_dah = 3·μ_dit` provisionally, flagged *unconfirmed* until
a mark lands ≥ `2·μ_dit` (then it re-anchors: `μ_dah = that duration`).

Special case: an all-dah opening (plausible for `CQ` at the margin) corrects
itself via the constraint clamp below the first time a real dit arrives —
the dit falls far below `μ_dit`, gets assigned to the dit cluster, drags it
down, and the clamp re-anchors `μ_dah`.

**Update** — per mark of duration `d`: assign to the nearer cluster in log
space (i.e. `dit` iff `d < B`), then EMA the assigned centroid:
`μ ← μ + 0.15·(d − μ)`.

**Constraints** — after every update enforce `2.2 ≤ μ_dah/μ_dit ≤ 4.5`
(weighted keying and Farnsworth stay inside this window); on violation,
re-anchor `μ_dah = 3·μ_dit`. Clamp `μ_dit` to `[20 ms, 150 ms]`
(60 WPM .. 8 WPM); PARIS WPM = `1200 / μ_dit_ms`, EMA-smoothed with
`α = 0.1` for reporting.

**Drift/regime change** — if 12 consecutive marks assign to a single cluster
*and* their coefficient of variation < 0.35 *and* their mean is off that
centroid by > 40 %, the operator has changed speed (QRQ/QRS): reinitialize
from the last 5 marks. (Plain EMA tracking already follows ≤ ~20 % gradual
drift; this rule catches step changes.)

### 4.2 Gap classification (spaces)

Nominal thresholds in dits (`u = gap_ms / μ_dit`):

- `u < 2.0` → **inter-element** (within a character)
- `2.0 ≤ u < 5.0` → **inter-character**
- `u ≥ 5.0` → **inter-word**

**[DEVIATION]** The implementation uses `1.6`, not `2.0`, for the
element/character boundary (`CHAR_GAP_DITS` in
`crates/manta-decode/src/timing.rs`) — see
`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`. §3.3's
hysteresis+debounce systematically inflates measured `μ_dit` relative to
true keyed timing without inflating gap durations the same way; at high WPM
that overshoot is large enough relative to the (short) true dit period that
real inter-character gaps can compute to under 2.0 dits and get merged into
the preceding character. `1.6` was chosen empirically (500-case sweep, two
independent seeds) as the value that captures the available fix with the
smallest deviation from the nominal `2.0`.

**Farnsworth decoupling** (ARCHITECTURE §5.3): run the same 2-means machinery
on gaps with `u ≥ 1.5` (the "long gaps"), yielding `μ_cgap` (character gap)
and `μ_wgap` (word gap) when bimodal. Once ≥ 8 long gaps have been observed
and `μ_wgap / μ_cgap ≥ 1.8`, the word threshold becomes the geometric mean
`sqrt(μ_cgap · μ_wgap)` instead of the fixed `5.0` dits; the character
threshold stays at `2.0` dits (element/character confusion is speed-locked;
character/word spacing is what Farnsworth stretches).

A trailing space reaching `7·μ_dit` without a new mark forces character +
word flush immediately (don't wait for the next mark to close a word —
spots must not lag the transmission).

### 4.3 Per-element likelihoods

Marks are modeled log-normally about their centroid with fixed log-domain
σ = 0.25:

```
ll(d | dit) = −(ln d − ln μ_dit)² / (2·0.25²)
ll(d | dah) = −(ln d − ln μ_dah)² / (2·0.25²)
```

(σ = 0.25 ⇒ ±28 % duration at 1σ — measured human keying jitter is 10–20 %,
QSB-induced edge erosion adds the rest. **This is the riskiest constant in
the spec**; it is config `decode.timing_sigma` and must be validated against
the golden corpus at M3.)

Spaces inside a character contribute no score (already classified by §4.2);
the character boundary decision itself is hard, not beamed — beaming gap
types explodes state for negligible gain at CW speeds.

### 4.4 Beam search over the Morse tree (width 4)

The code tree: root, dit = left child, dah = right child; nodes carry an
optional glyph. Standard table A–Z, 0–9, `. , ? / = + - ( ) @ : ; ' " _ $ !`
plus prosign terminal nodes: `AR` (`.-.-.`), `SK` (`...-.-`), `BT` (`-...-`,
same node as `=`, emitted as `=`), `KN` (`-.--.`, same node as `(`), `AS`
(`.-...`), `VE/SN` (`...-.`). Prosigns emit as text tokens `<AR>` `<SK>`
`<AS>` `<SN>` in the JSON stream and are dropped from the telnet-facing text.

**[DEVIATION — narrowed]** The beam is **character-local**: it resets to the
tree root at every character boundary, and inter-character continuity is
greedy (winning character is committed). ARCHITECTURE §5.4 could be read as a
transmission-length beam; word-level ambiguity is the validator's job
(cty.dat / SCP context in `manta-spot`), and a character-local beam makes
determinism and confidence bookkeeping trivial. Cross-character correction is
explicitly out of scope for the classical decoder.

Per character:

1. Start with one hypothesis: `(node = root, score = 0)`.
2. For each mark `d` in the character: every hypothesis branches to its
   dit-child (score `+ ll(d|dit)`) and dah-child (score `+ ll(d|dah)`).
   A branch into a nonexistent child (sequence longer than any code, > 7
   elements) is dropped; if *all* branches drop, the character aborts as
   garble (emits nothing, counts as a decode error for confidence).
3. Prune to the best **4** hypotheses by score after each mark.
4. At the character boundary: surviving hypotheses whose node has no glyph
   are dropped; if all four are glyphless, the character emits `?` with
   confidence 0. The winner is the highest-score glyph-bearing hypothesis.

**Error prosign:** a mark run of ≥ 6 dits-classified marks with no dah
(operator sending `........`) emits control token `<ERR>`; the validator
discards the current word buffer back to the previous word boundary.

### 4.5 Per-character confidence

Softmax over the final hypothesis scores `s₁ ≥ s₂ ≥ …` (the ≤ 4 survivors,
plus dropped-at-boundary hypotheses excluded):

```
c_char = exp(s₁) / Σᵢ exp(sᵢ)          (∈ (0, 1], =1 if single survivor)
c_char ← c_char · q,  q = clamp(SNR_2500 / 20 dB, 0.3, 1.0)
```

`q` folds channel quality in so that a clean-timed character in the mud never
reaches full confidence. Emitted per character in the decoder output stream.

### 4.6 Per-callsign confidence (consumed by `manta-spot`)

For a candidate callsign of `n` characters with confidences `c₁..c_n`,
decoded `r` distinct times on the track within the 90 s window:

```
c_call = (Π cᵢ)^(1/n) · (1 − 0.5^r)
```

Geometric mean (one garbled character tanks it, correctly) times a
repetition factor: `r=1 → 0.5`, `r=2 → 0.75`, `r=3 → 0.875`. The validator's
own adjustments (cty/SCP hits, per ARCHITECTURE §6) multiply on top of this
and are specified in `manta-spot`, not here. The ≥ 2-repetition gate for
first spot is unchanged for non-beacon, non-allowlisted spot types; a
message already type-tagged `BEACON` by the context parse (ARCHITECTURE §6
step 1), or a callsign the operator has explicitly allowlisted (ARCHITECTURE
§6's Watch List), is exempt from this gate and may spot on its first decode
(MAN-28) — `r` still feeds `c_call` above unchanged, so a single-decode spot
of either kind still carries the `r=1` confidence penalty. An allowlisted
callsign also bypasses ARCHITECTURE §6 steps 1 (context parse -- tagged
`SpotType::Unknown` when no CQ/DE/UP/beacon pattern matched) and 2
(grammar/cty) entirely.

---

## 5. Decoder output

Per track, an ordered event stream:

```
CharDecoded { track_id, sample_ts: u64, char: char | Token, confidence: f32 }
WordBoundary { track_id, sample_ts: u64 }
SpeedUpdate { track_id, wpm: f32 }          (emitted on ≥ 1 WPM change)
TrackMeta   { track_id, snr_2500_db: f32, freq_centroid: f64 }  (1 Hz cadence)
```

`sample_ts` is the input-stream sample counter (u64, monotonic from stream
start). Wall-clock time exists only at the spot-emission boundary
(`manta-server`), derived as `stream_start_time + sample_ts / fs` where
`stream_start_time` comes from config/file sidecar — never from `Instant::now()`
inside the decode path.

---

## 6. Determinism requirements

A daemon run from an IQ file MUST produce byte-identical decoder output (and
therefore spot logs) across runs and platforms (ARCHITECTURE §9). Normative
rules:

1. **No RNG** anywhere in `manta-dsp` / `manta-decode`. (The testkit's
   jitter models use seeded `rand_chacha`; seeds are part of the test vector.)
2. **No wall clock** in the decode path (§5). All timers are hop/sample
   counters.
3. **Fixed iteration order:** tracks are processed in ascending birth order
   (track_id, a monotonic u32) each hop; channels in ascending index. Any map
   keyed by track/channel in an output-affecting path is a `BTreeMap` or
   sorted `Vec`, never a `HashMap` iterated.
4. **Float discipline:** all per-sample state is `f32` with a fixed operation
   order (no fast-math, no FMA-dependent reductions: the fold in §1.3 and the
   centroid in §1.4 accumulate in `f64` sequentially). `rustfft` is
   deterministic per-platform for power-of-two sizes; cross-platform FFT
   bit-equality is NOT assumed — the byte-identical requirement applies to
   like-for-like builds, and cross-platform equality is asserted at the
   *decoded-text* level (test vectors, §7), not the sample level.
5. **Beam tie-break:** equal scores order by (element-sequence lexical order,
   dit < dah). Softmax computed with the max-subtraction trick, fixed order.
6. **Pool scheduling must not affect output:** decoder workers may run in any
   order, but each track's decoder is a pure function of its own input queue;
   emitted events are sequenced by `(sample_ts, track_id)` before the
   validator sees them.

CI enforces: same binary + same IQ file, 3 runs → identical SHA-256 of the
JSON spot log; and the §7 vectors on all platforms.

---

## 7. Golden test vectors (M0/M1 acceptance)

All generated by `manta-testkit`: text → keyed envelope (raised-cosine
edges, 5 ms rise/fall, per-element timing jitter σ = 8 % where stated) →
complex tone at the stated offset → impairments via `coppa-channel`
(`awgn(seed)`; Watterson via the streaming `WattersonChannel` API per
SPEC-watterson §6 — not the deprecated one-shot helper). SNR is quoted
**in 2500 Hz**.
Every vector: `fs = 96 000`, 120 s scene unless stated, fixed seeds recorded
in the fixture manifest. Text payload (unless stated):
`CQ CQ DE <CALL> <CALL> K` repeated for the scene duration.

| # | Name | Signal(s) | Impairment | Pass criteria |
|---|---|---|---|---|
| V1 | clean-20 | 20 WPM, +20 dB, offset +12.34 kHz, W1AW | AWGN only, no jitter | char accuracy = 100 %; 1 track; freq error ≤ 10 Hz |
| V2 | fast-35 | 35 WPM, +15 dB, JA1ABC | AWGN, jitter 8 % | char ≥ 99 %; WPM reported 35 ± 2 |
| V3 | slow-weak | 12 WPM, +6 dB, VK9DX | AWGN, jitter 8 % | char ≥ 95 %; callsign validated (≥ 2 reps) |
| V4 | fade-good | 25 WPM, +10 dB, DL1ABC | Watterson CCIR-good | char ≥ 95 % |
| V5 | fade-poor | 22 WPM, +3 dB, ZL2XYZ | Watterson CCIR-poor | char ≥ 80 %; callsign validated within 90 s |
| V6 | qsb-sine | 20 WPM, envelope ×(0.55 + 0.45·sin 2π·0.2t) (≈ +20→0 dB), K5ZZZ | AWGN | char ≥ 90 %; track survives (no CLOSED before end) |
| V7 | adjacent | 24 WPM @ +10.000 kHz and 28 WPM @ +10.150 kHz, both +15 dB, calls N1AA / N2BB | AWGN | exactly 2 tracks; both char ≥ 95 %; both freqs ± 15 Hz |
| V8 | pileup-50 | 50 signals, 10–35 WPM, −2..+25 dB, uniform over ±45 kHz, unique calls from fixture list | AWGN, jitter 8 % | ≥ 45/50 callsigns validated in 120 s; 0 bogus (non-fixture) callsigns spotted |
| V8w | pileup-50-fading | same scene as V8 | Watterson CCIR-poor, jitter 8 % | ≥ 90 % of signals with mean SNR ≥ +6 dB decoded with CER < 10 %; 0 bogus callsigns; 0 cross-channel ghost decodes |
| V9 | drift | 18 WPM, +12 dB, drift +50 Hz/min, EA8AAA | AWGN | 1 track (no split); char ≥ 90 %; final freq tracks within 15 Hz |
| V10 | farnsworth | 15 WPM chars / 25 WPM char-speed (Farnsworth), +15 dB, G4XXX | AWGN | char ≥ 95 %; word boundaries 100 % correct |

M0 = V1 passing end-to-end from a WAV file. M1 = V1–V6. V7–V10 and V8w gate M2
(multi-track engine). The RBN-parity corpus benchmark remains the M3 gate
(ARCHITECTURE §9) and is not redefined here.

### 7.1 `manta-spot` validator vectors (M3 sub-project 1)

Unlike V1–V10 (testkit-synthesized IQ), these operate at the
`DecoderEvent`-stream level -- hand-built event sequences feeding
`Validator::ingest` directly, no IQ synthesis involved. V11-V15, V18-V21
are implemented in `crates/manta-spot/tests/golden_v11_v15.rs`; V16-V17
(operator suppression, MAN-31 -- orthogonal to this pipeline, see
ARCHITECTURE §6) in `crates/manta-spot/tests/golden_v16_v17.rs`.

| # | Name | Scenario | Pass criteria |
|---|---|---|---|
| V11 | context-parse | Each of `CQ <call>`, `CQ TEST <call>`, `DE <call>`, `<call> UP`, `V V V <call>` | Correct `SpotType` assigned per pattern family |
| V12 | bogus-prefix | Structurally-valid callsign with a prefix absent from cty.dat | 0 spots, even though grammar passes |
| V13 | scp-boost | Same callsign/confidences with vs. without SCP membership | `c_call` strictly higher when a member; absence never rejects |
| V14 | repetition-gate | 1 decode vs. 2 decodes of the same callsign within 90 s, non-beacon spot type | 1 rep never spots; 2 reps does |
| V15 | dedupe | Repeat spot inside the 10 min window, then an SNR jump >= 6 dB | Suppressed inside the window; allowed after the SNR jump |
| V16 | bad-call blocklist | Callsign present vs. absent from the operator's bad-call list | Present → 0 spots; absent → spots normally |
| V17 | notched frequency | Track frequency inside vs. outside a notched range | Inside → 0 spots; outside → spots normally |
| V18 | beacon-repetition-exemption | 1 decode of a `V V V <call>` beacon pattern | `BEACON`-tagged spot emits on the first decode, gate not applied (MAN-28) |
| V19 | allowlist-bypass | A single decode of a callsign with an unallocated cty prefix, explicitly allowlisted | Spots despite failing grammar/cty and despite only 1 decode (MAN-28 Watch List) |
| V20 | allowlist-no-context | An allowlisted callsign decoded with no CQ/DE/UP/beacon framing at all | Spots, tagged `SpotType::Unknown` (MAN-28 Watch List, the primary NCDXF-beacon case) |
| V21 | allowlist-independent-of-context | A stale, already-attempted context match (e.g. `CQ K5ARH`, decoded once, never spotted) sits in the window when a different, freshly-allowlisted word arrives | The allowlisted word still spots -- context-match and allowlist candidates are evaluated independently, not one-or-the-other by priority (MAN-28 Watch List) |

---

## 8. Module map (where each section lands)

| Spec section | Crate::module |
|---|---|
| §1 channelizer, prototype | `manta-dsp::pfb`, `manta-dsp::proto` |
| §1.4 interpolation | `manta-dsp::centroid` |
| §2 floor + gate + state machine | `manta-dsp::floor`, `manta-engine::track` |
| §3 demod | `manta-decode::envelope` |
| §4.1–4.2 timing | `manta-decode::timing` |
| §4.3–4.5 beam decode | `manta-decode::beam`, `manta-decode::tree` |
| §5 events | `manta-decode::events` |
| §7 vectors | `manta-testkit::vectors` |

## 9. Configuration keys

All normative constants above, with defaults:

```toml
[detector]
on_snr_db = 6.0        off_snr_db = 3.0
confirm_ms = 50        hang_ms = 5000
gc_ms = 30000          warmup_ms = 2000
floor_quantile = 0.25  floor_window_ms = 10000
block_channels = 32    block_allowance_db = 3.0

[decode]
timing_sigma = 0.25    beam_width = 4
debounce_ms = 12       hyst_up = 1.25       hyst_down = 0.80
tau_lo_ms = 500        tau_hi_bounds_ms = [100, 400]
mu_ratio_bounds = [2.2, 4.5]
char_gap_dits = 2.0    word_gap_dits = 5.0  flush_gap_dits = 7.0
cluster_alpha = 0.15

[input]
# Per-source oscillator drift correction, ppm; range [-1000, 1000]
# (`manta_spot::calibration_factor_from_ppm`). §1.4, MAN-29.
freq_correction_ppm = 0.0

[spot]
# Operator Watch List (§6, MAN-28): callsigns here bypass grammar/cty
# validation and the repetition gate entirely in manta-spot's validator.
allowlist = []
```

## 10. Deviations from ARCHITECTURE.md

1. **Kaiser prototype is new code, not reused** (§1.2): `coppa-dsp::filter`
   only ships `RrcFilter`; the reuse table's "FIR design → coppa-dsp::filter"
   row is wrong for the PFB prototype. `FftProcessor` reuse stands.
2. **`AdaptiveAgc` dropped from the decode path** (§3.1): the dual-EMA
   threshold is self-normalizing; coppa's block AGC adds latency and couples
   with keying. Replaced by a per-track fixed reference scale.
3. **Beam search is character-local** (§4.4): resets at character boundaries;
   greedy across characters. Word-level context belongs to the validator.
4. Stopband target tightened from the implied ~60 dB to **80 dB** (§1.2) —
   free given 8 taps/branch, and pileup scenes (V8) have ≥ 27 dB dynamic
   range between neighbors.
